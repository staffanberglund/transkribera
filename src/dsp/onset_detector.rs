use std::{collections::VecDeque, sync::Arc};

use anyhow::{Result, bail};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use super::window::periodic_hann;

const FLUX_THRESHOLD: f32 = 0.40;
const MIN_MEAN_MAGNITUDE: f32 = 1e-5;
const RETRIGGER_SECONDS: f32 = 0.030;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnsetDetectorConfig {
    pub sample_rate: u32,
    pub fft_size: usize,
    pub analysis_hop: usize,
    pub channel_count: usize,
}

struct ChannelState {
    input: VecDeque<f32>,
    spectrum: Vec<Complex32>,
    previous_magnitude: Vec<f32>,
    initialized: bool,
}

impl ChannelState {
    fn new(fft_size: usize) -> Self {
        Self {
            input: VecDeque::with_capacity(fft_size * 2),
            spectrum: vec![Complex32::default(); fft_size],
            previous_magnitude: vec![0.0; fft_size / 2 + 1],
            initialized: false,
        }
    }

    fn reset(&mut self) {
        self.input.clear();
        self.spectrum.fill(Complex32::default());
        self.previous_magnitude.fill(0.0);
        self.initialized = false;
    }
}

/// Detects broadband onsets once on the original signal and timestamps them on
/// the source-frame timeline. A multiband processor can then give every
/// analysis resolution the same event schedule.
pub struct OnsetDetector {
    fft_size: usize,
    analysis_hop: usize,
    channel_count: usize,
    window: Vec<f32>,
    forward_fft: Arc<dyn Fft<f32>>,
    channels: Vec<ChannelState>,
    next_frame_start: u64,
    source_frames_since_onset: usize,
    retrigger_frames: usize,
}

impl OnsetDetector {
    pub fn new(config: OnsetDetectorConfig) -> Result<Self> {
        if config.sample_rate == 0 {
            bail!("onset detector sample rate must be greater than zero");
        }
        if !(1..=2).contains(&config.channel_count) {
            bail!("onset detector supports one or two channels");
        }
        if config.fft_size < 2 || !config.fft_size.is_power_of_two() {
            bail!("onset detector FFT size must be a power of two greater than one");
        }
        if config.analysis_hop == 0 || config.analysis_hop > config.fft_size {
            bail!("onset detector hop must be between one and the FFT size");
        }

        let mut planner = FftPlanner::<f32>::new();
        Ok(Self {
            fft_size: config.fft_size,
            analysis_hop: config.analysis_hop,
            channel_count: config.channel_count,
            window: periodic_hann(config.fft_size),
            forward_fft: planner.plan_fft_forward(config.fft_size),
            channels: (0..config.channel_count)
                .map(|_| ChannelState::new(config.fft_size))
                .collect(),
            next_frame_start: 0,
            source_frames_since_onset: usize::MAX,
            retrigger_frames: (config.sample_rate as f32 * RETRIGGER_SECONDS).ceil() as usize,
        })
    }

    pub fn process(&mut self, input: &[f32], onset_frames: &mut Vec<u64>) -> Result<()> {
        if !input.len().is_multiple_of(self.channel_count) {
            bail!("onset detector input does not contain complete audio frames");
        }
        for frame in input.chunks_exact(self.channel_count) {
            for (channel, sample) in self.channels.iter_mut().zip(frame) {
                channel.input.push_back(*sample);
            }
        }

        while self
            .channels
            .first()
            .is_some_and(|channel| channel.input.len() >= self.fft_size)
        {
            self.process_frame(onset_frames);
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.reset();
        }
        self.next_frame_start = 0;
        self.source_frames_since_onset = usize::MAX;
    }

    fn process_frame(&mut self, onset_frames: &mut Vec<u64>) {
        let bins = self.fft_size / 2 + 1;
        let mut strongest_channel_flux = 0.0_f32;
        for channel in &mut self.channels {
            for (index, (sample, window)) in channel
                .input
                .iter()
                .take(self.fft_size)
                .zip(&self.window)
                .enumerate()
            {
                channel.spectrum[index] = Complex32::new(sample * window, 0.0);
            }
            self.forward_fft.process(&mut channel.spectrum);

            let mut positive_flux = 0.0;
            let mut magnitude_sum = 0.0;
            for bin in 0..bins {
                let magnitude = channel.spectrum[bin].norm();
                magnitude_sum += magnitude;
                if channel.initialized {
                    positive_flux += (magnitude - channel.previous_magnitude[bin]).max(0.0);
                }
                channel.previous_magnitude[bin] = magnitude;
            }
            let mean_magnitude = magnitude_sum / bins as f32;
            if channel.initialized && mean_magnitude >= MIN_MEAN_MAGNITUDE {
                strongest_channel_flux =
                    strongest_channel_flux.max(positive_flux / magnitude_sum.max(f32::EPSILON));
            }
            channel.initialized = true;
            for _ in 0..self.analysis_hop {
                channel.input.pop_front();
            }
        }

        self.source_frames_since_onset = self
            .source_frames_since_onset
            .saturating_add(self.analysis_hop);
        if strongest_channel_flux >= FLUX_THRESHOLD
            && self.source_frames_since_onset >= self.retrigger_frames
        {
            onset_frames.push(self.next_frame_start + self.fft_size as u64 / 2);
            self.source_frames_since_onset = 0;
        }
        self.next_frame_start += self.analysis_hop as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::{OnsetDetector, OnsetDetectorConfig};

    fn detector() -> OnsetDetector {
        OnsetDetector::new(OnsetDetectorConfig {
            sample_rate: 8_000,
            fft_size: 128,
            analysis_hop: 16,
            channel_count: 2,
        })
        .unwrap()
    }

    #[test]
    fn separated_stereo_attacks_produce_ordered_source_timestamps() {
        let mut input = vec![0.0; 8_000 * 2];
        for frame in [2_000, 5_000] {
            for age in 0..64 {
                input[(frame + age) * 2] = if age.is_multiple_of(2) { 0.8 } else { -0.8 };
            }
        }
        let mut detector = detector();
        let mut onsets = Vec::new();
        for chunk in input.chunks(74) {
            detector.process(chunk, &mut onsets).unwrap();
        }

        assert!(onsets.len() >= 2, "detected onsets: {onsets:?}");
        assert!(onsets.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn reset_restarts_the_source_timeline() {
        let mut input = vec![0.0; 2_000 * 2];
        for age in 0..64 {
            input[(1_000 + age) * 2] = if age.is_multiple_of(2) { 0.8 } else { -0.8 };
        }
        let mut detector = detector();
        let mut first = Vec::new();
        detector.process(&input, &mut first).unwrap();
        detector.reset();
        let mut second = Vec::new();
        detector.process(&input, &mut second).unwrap();
        assert_eq!(first, second);
    }
}
