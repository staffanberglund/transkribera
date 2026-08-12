use std::{
    collections::VecDeque,
    f32::consts::{PI, TAU},
    sync::Arc,
};

use anyhow::{Result, bail};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use super::window::periodic_hann;

/// Time required for an exponential speed transition to cover about 63% of
/// the distance to its target, measured on the source timeline.
const SPEED_SMOOTHING_TIME_SECONDS: f32 = 0.050;
// Values this close to unity are canonicalized so the transparent phase path
// cannot be missed because of slider or property conversion roundoff.
const UNITY_SPEED_EPSILON: f32 = 1e-4;
// Positive spectral flux is normalized by the current frame magnitude. A
// fairly conservative threshold avoids treating normal vibrato/noise movement
// as a new attack.
const TRANSIENT_FLUX_THRESHOLD: f32 = 0.40;
const TRANSIENT_MIN_MEAN_MAGNITUDE: f32 = 1e-5;
const TRANSIENT_RETRIGGER_SECONDS: f32 = 0.030;
// Avoid amplifying FFT roundoff at the nearly-zero edges of the final window.
const NORMALIZATION_EPSILON: f32 = 1e-4;

/// Everything needed to construct one independent phase-vocoder processing unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaseVocoderConfig {
    pub sample_rate: u32,
    pub fft_size: usize,
    pub analysis_hop: usize,
    pub channel_count: usize,
    pub playback_speed: f32,
}

struct ChannelState {
    input: VecDeque<f32>,
    spectrum: Vec<Complex32>,
    magnitude: Vec<f32>,
    analysis_phase: Vec<f32>,
    propagated_phase: Vec<f32>,
    spectral_peaks: Vec<usize>,
    previous_magnitude: Vec<f32>,
    previous_phase: Vec<f32>,
    synthesis_phase: Vec<f32>,
    overlap: VecDeque<f32>,
    overlap_weight: VecDeque<f32>,
    phase_initialized: bool,
}

impl ChannelState {
    fn new(fft_size: usize) -> Self {
        let bins = fft_size / 2 + 1;
        Self {
            input: VecDeque::with_capacity(fft_size * 2),
            spectrum: vec![Complex32::default(); fft_size],
            magnitude: vec![0.0; bins],
            analysis_phase: vec![0.0; bins],
            propagated_phase: vec![0.0; bins],
            spectral_peaks: Vec::with_capacity(bins),
            previous_magnitude: vec![0.0; bins],
            previous_phase: vec![0.0; bins],
            synthesis_phase: vec![0.0; bins],
            overlap: VecDeque::from(vec![0.0; fft_size]),
            overlap_weight: VecDeque::from(vec![0.0; fft_size]),
            phase_initialized: false,
        }
    }

    fn reset(&mut self, fft_size: usize) {
        self.input.clear();
        self.spectrum.fill(Complex32::default());
        self.magnitude.fill(0.0);
        self.analysis_phase.fill(0.0);
        self.propagated_phase.fill(0.0);
        self.spectral_peaks.clear();
        self.previous_magnitude.fill(0.0);
        self.previous_phase.fill(0.0);
        self.synthesis_phase.fill(0.0);
        self.overlap.clear();
        self.overlap.resize(fft_size, 0.0);
        self.overlap_weight.clear();
        self.overlap_weight.resize(fft_size, 0.0);
        self.phase_initialized = false;
    }
}

/// A conventional STFT phase vocoder for interleaved `f32` PCM.
///
/// `playback_speed` means output tempo divided by source tempo: `0.5` produces
/// approximately twice as many output samples, while `1.5` produces roughly
/// two thirds as many. Consequently `synthesis_hop = analysis_hop / speed`.
pub struct PhaseVocoder {
    sample_rate: u32,
    channel_count: usize,
    fft_size: usize,
    analysis_hop: usize,
    window: Vec<f32>,
    forward_fft: Arc<dyn Fft<f32>>,
    inverse_fft: Arc<dyn Fft<f32>>,
    channel_state: Vec<ChannelState>,
    target_playback_speed: f32,
    current_playback_speed: f32,
    speed_smoothing_coefficient: f32,
    synthesis_hop_fraction: f32,
    source_frames_since_transient: usize,
    transient_retrigger_frames: usize,
    detected_transients: u64,
    external_transient_timeline: bool,
    pending_external_transients: VecDeque<u64>,
    last_external_transient: Option<u64>,
    external_tempo_origin_frames: Option<usize>,
    speed_events: Vec<(u64, f32)>,
    input_frames_received: u64,
    pending_real_frames: usize,
    processed_frames: u64,
}

impl PhaseVocoder {
    pub fn new(config: PhaseVocoderConfig) -> Result<Self> {
        let PhaseVocoderConfig {
            sample_rate,
            fft_size,
            analysis_hop,
            channel_count,
            playback_speed,
        } = config;
        if sample_rate == 0 {
            bail!("sample rate must be greater than zero");
        }
        if !(1..=2).contains(&channel_count) {
            bail!("phase vocoder supports one or two channels, not {channel_count}");
        }
        if fft_size < 2 || !fft_size.is_power_of_two() {
            bail!("FFT size must be a power of two greater than one");
        }
        if analysis_hop == 0 || analysis_hop > fft_size {
            bail!("analysis hop must be between one and the FFT size");
        }
        validate_playback_speed(playback_speed)?;

        let mut planner = FftPlanner::<f32>::new();
        let forward_fft = planner.plan_fft_forward(fft_size);
        let inverse_fft = planner.plan_fft_inverse(fft_size);

        Ok(Self {
            sample_rate,
            channel_count,
            fft_size,
            analysis_hop,
            window: periodic_hann(fft_size),
            forward_fft,
            inverse_fft,
            channel_state: (0..channel_count)
                .map(|_| ChannelState::new(fft_size))
                .collect(),
            target_playback_speed: canonical_playback_speed(playback_speed),
            current_playback_speed: canonical_playback_speed(playback_speed),
            speed_smoothing_coefficient: speed_smoothing_coefficient(sample_rate, analysis_hop),
            synthesis_hop_fraction: 0.0,
            source_frames_since_transient: usize::MAX,
            transient_retrigger_frames: (sample_rate as f32 * TRANSIENT_RETRIGGER_SECONDS).ceil()
                as usize,
            detected_transients: 0,
            external_transient_timeline: false,
            pending_external_transients: VecDeque::new(),
            last_external_transient: None,
            external_tempo_origin_frames: None,
            speed_events: vec![(0, canonical_playback_speed(playback_speed))],
            input_frames_received: 0,
            pending_real_frames: 0,
            processed_frames: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn latency_frames(&self) -> usize {
        self.fft_size - self.analysis_hop
    }

    #[cfg(test)]
    pub(crate) fn detected_transients(&self) -> u64 {
        self.detected_transients
    }

    pub fn set_playback_speed(&mut self, playback_speed: f32) -> Result<()> {
        validate_playback_speed(playback_speed)?;
        let playback_speed = canonical_playback_speed(playback_speed);
        self.target_playback_speed = playback_speed;
        if self.external_tempo_origin_frames.is_some()
            && self
                .speed_events
                .last()
                .is_none_or(|(_, speed)| *speed != playback_speed)
        {
            if self
                .speed_events
                .last()
                .is_some_and(|(frame, _)| *frame == self.input_frames_received)
            {
                if let Some((_, speed)) = self.speed_events.last_mut() {
                    *speed = playback_speed;
                }
            } else {
                self.speed_events
                    .push((self.input_frames_received, playback_speed));
            }
        }
        if self.processed_frames == 0 {
            self.current_playback_speed = playback_speed;
        }
        Ok(())
    }

    pub fn use_external_transient_timeline(&mut self) {
        self.external_transient_timeline = true;
        self.pending_external_transients.clear();
        self.last_external_transient = None;
    }

    pub fn use_external_tempo_timeline(&mut self, output_origin_frames: usize) {
        self.use_external_transient_timeline();
        self.external_tempo_origin_frames = Some(output_origin_frames);
        self.speed_events.clear();
        self.speed_events.push((0, self.current_playback_speed));
        self.input_frames_received = 0;
    }

    pub fn process_with_transients(
        &mut self,
        input: &[f32],
        transient_source_frames: &[u64],
        output: &mut Vec<f32>,
    ) -> Result<()> {
        if !self.external_transient_timeline {
            bail!("external transient timeline has not been enabled");
        }
        if transient_source_frames
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            bail!("external transient timestamps must be strictly increasing");
        }
        if let (Some(previous), Some(next)) = (
            self.last_external_transient,
            transient_source_frames.first(),
        ) && *next <= previous
        {
            bail!("external transient timestamps must remain increasing across buffers");
        }
        if let Some(last) = transient_source_frames.last() {
            self.last_external_transient = Some(*last);
        }
        self.pending_external_transients
            .extend(transient_source_frames.iter().copied());
        self.process(input, output)
    }

    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()> {
        if !input.len().is_multiple_of(self.channel_count) {
            bail!("interleaved input does not contain complete audio frames");
        }

        let frames = input.len() / self.channel_count;
        self.input_frames_received = self.input_frames_received.saturating_add(frames as u64);
        self.pending_real_frames = self.pending_real_frames.saturating_add(frames);
        for frame in input.chunks_exact(self.channel_count) {
            for (channel, sample) in self.channel_state.iter_mut().zip(frame) {
                channel.input.push_back(*sample);
            }
        }
        self.process_available(output);
        Ok(())
    }

    pub fn flush(&mut self, output: &mut Vec<f32>) -> Result<()> {
        while self.pending_real_frames > 0 {
            for channel in &mut self.channel_state {
                channel.input.resize(self.fft_size, 0.0);
            }
            self.process_frame(output);
        }

        let remaining = self
            .channel_state
            .first()
            .map_or(0, |channel| channel.overlap.len());
        self.emit_output(remaining, output);
        Ok(())
    }

    pub fn reset(&mut self) {
        for channel in &mut self.channel_state {
            channel.reset(self.fft_size);
        }
        self.current_playback_speed = self.target_playback_speed;
        self.synthesis_hop_fraction = 0.0;
        self.source_frames_since_transient = usize::MAX;
        self.detected_transients = 0;
        self.pending_external_transients.clear();
        self.last_external_transient = None;
        self.speed_events.clear();
        self.speed_events.push((0, self.target_playback_speed));
        self.input_frames_received = 0;
        self.pending_real_frames = 0;
        self.processed_frames = 0;
    }

    fn process_available(&mut self, output: &mut Vec<f32>) {
        while self
            .channel_state
            .first()
            .is_some_and(|channel| channel.input.len() >= self.fft_size)
        {
            self.process_frame(output);
        }
    }

    fn process_frame(&mut self, output: &mut Vec<f32>) {
        if self.external_tempo_origin_frames.is_none() && self.processed_frames > 0 {
            self.current_playback_speed += (self.target_playback_speed
                - self.current_playback_speed)
                * self.speed_smoothing_coefficient;
            if (self.target_playback_speed - self.current_playback_speed).abs() < 1e-4 {
                self.current_playback_speed = self.target_playback_speed;
            }
        }

        let synthesis_hop = if let Some(origin) = self.external_tempo_origin_frames {
            let analysis_center = self.processed_frames as f64 * self.analysis_hop as f64
                + self.fft_size as f64 / 2.0;
            let current_start =
                origin as f64 + self.map_source_frame(analysis_center) - self.fft_size as f64 / 2.0;
            let next_start = origin as f64
                + self.map_source_frame(analysis_center + self.analysis_hop as f64)
                - self.fft_size as f64 / 2.0;
            if self.processed_frames == 0 {
                self.emit_output(current_start.round().max(0.0) as usize, output);
            }
            let exact_hop = (next_start - current_start).max(1.0);
            self.current_playback_speed = self.analysis_hop as f32 / exact_hop as f32;
            (next_start.round() - current_start.round()).max(1.0) as usize
        } else {
            let synthesis_hop_exact = self.analysis_hop as f32 / self.current_playback_speed;
            let synthesis_hop_with_fraction = synthesis_hop_exact + self.synthesis_hop_fraction;
            let synthesis_hop = synthesis_hop_with_fraction.floor().max(1.0) as usize;
            self.synthesis_hop_fraction = synthesis_hop_with_fraction - synthesis_hop as f32;
            synthesis_hop
        };
        let transparent_unity = self.current_playback_speed == 1.0
            && self.target_playback_speed == 1.0
            && synthesis_hop == self.analysis_hop;
        let bins = self.fft_size / 2 + 1;

        let mut strongest_channel_flux = 0.0_f32;
        for channel in &mut self.channel_state {
            let mut positive_spectral_flux = 0.0;
            let mut current_magnitude_sum = 0.0;
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

            for bin in 0..bins {
                let input_bin = channel.spectrum[bin];
                let phase = input_bin.arg();
                channel.magnitude[bin] = input_bin.norm();
                if !self.external_transient_timeline {
                    current_magnitude_sum += channel.magnitude[bin];
                }
                if !self.external_transient_timeline && channel.phase_initialized {
                    positive_spectral_flux +=
                        (channel.magnitude[bin] - channel.previous_magnitude[bin]).max(0.0);
                }
                channel.analysis_phase[bin] = phase;
                if channel.phase_initialized {
                    let expected_phase_advance =
                        TAU * bin as f32 * self.analysis_hop as f32 / self.fft_size as f32;
                    let deviation =
                        wrap_phase(phase - channel.previous_phase[bin] - expected_phase_advance);
                    let instantaneous_angular_frequency = TAU * bin as f32 / self.fft_size as f32
                        + deviation / self.analysis_hop as f32;
                    channel.propagated_phase[bin] +=
                        instantaneous_angular_frequency * synthesis_hop as f32;
                } else {
                    channel.propagated_phase[bin] = phase;
                }
                channel.previous_phase[bin] = phase;
            }
            if !self.external_transient_timeline {
                channel
                    .previous_magnitude
                    .copy_from_slice(&channel.magnitude);
                let mean_magnitude = current_magnitude_sum / bins as f32;
                if channel.phase_initialized && mean_magnitude >= TRANSIENT_MIN_MEAN_MAGNITUDE {
                    strongest_channel_flux = strongest_channel_flux
                        .max(positive_spectral_flux / current_magnitude_sum.max(f32::EPSILON));
                }
            }
        }

        let transient_detected = if self.external_transient_timeline {
            self.take_external_transient_for_current_frame()
        } else {
            self.source_frames_since_transient = self
                .source_frames_since_transient
                .saturating_add(self.analysis_hop);
            strongest_channel_flux >= TRANSIENT_FLUX_THRESHOLD
                && self.source_frames_since_transient >= self.transient_retrigger_frames
        };
        let transient = transient_detected && !transparent_unity;
        if transient {
            self.source_frames_since_transient = 0;
            self.detected_transients += 1;
        }

        for channel in &mut self.channel_state {
            if transparent_unity || transient || !channel.phase_initialized {
                channel
                    .propagated_phase
                    .copy_from_slice(&channel.analysis_phase);
                channel
                    .synthesis_phase
                    .copy_from_slice(&channel.analysis_phase);
            } else {
                identity_phase_lock(
                    &channel.magnitude,
                    &channel.analysis_phase,
                    &channel.propagated_phase,
                    &mut channel.synthesis_phase,
                    &mut channel.spectral_peaks,
                );
            }

            for bin in 0..bins {
                channel.spectrum[bin] =
                    Complex32::from_polar(channel.magnitude[bin], channel.synthesis_phase[bin]);
            }
            channel.phase_initialized = true;

            for bin in 1..self.fft_size / 2 {
                channel.spectrum[self.fft_size - bin] = channel.spectrum[bin].conj();
            }
            self.inverse_fft.process(&mut channel.spectrum);

            channel.overlap.resize(self.fft_size, 0.0);
            channel.overlap_weight.resize(self.fft_size, 0.0);
            let inverse_scale = 1.0 / self.fft_size as f32;
            for index in 0..self.fft_size {
                channel.overlap[index] +=
                    channel.spectrum[index].re * inverse_scale * self.window[index];
                channel.overlap_weight[index] += self.window[index] * self.window[index];
            }
            for _ in 0..self.analysis_hop.min(channel.input.len()) {
                channel.input.pop_front();
            }
        }

        self.pending_real_frames = self.pending_real_frames.saturating_sub(self.analysis_hop);
        self.processed_frames += 1;
        self.emit_output(synthesis_hop, output);
    }

    fn take_external_transient_for_current_frame(&mut self) -> bool {
        let frame_start = self
            .processed_frames
            .saturating_mul(self.analysis_hop as u64);
        let frame_center = frame_start.saturating_add(self.fft_size as u64 / 2);
        let right_boundary = frame_center.saturating_add(self.analysis_hop as u64 / 2);
        let mut transient = false;
        while self
            .pending_external_transients
            .front()
            .is_some_and(|timestamp| *timestamp <= right_boundary)
        {
            self.pending_external_transients.pop_front();
            transient = true;
        }
        transient
    }

    fn map_source_frame(&self, source_frame: f64) -> f64 {
        let mut output_frame = 0.0;
        let mut segment_start = 0.0;
        let mut speed = self.speed_events[0].1 as f64;
        for &(event_frame, event_speed) in self.speed_events.iter().skip(1) {
            let event_frame = event_frame as f64;
            if event_frame >= source_frame {
                break;
            }
            output_frame += (event_frame - segment_start) / speed;
            segment_start = event_frame;
            speed = event_speed as f64;
        }
        output_frame + (source_frame - segment_start) / speed
    }

    fn emit_output(&mut self, frames: usize, output: &mut Vec<f32>) {
        output.reserve(frames.saturating_mul(self.channel_count));
        for _ in 0..frames {
            for channel in &mut self.channel_state {
                let sample = channel.overlap.pop_front().unwrap_or(0.0);
                let weight = channel.overlap_weight.pop_front().unwrap_or(0.0);
                output.push(if weight > NORMALIZATION_EPSILON {
                    sample / weight
                } else {
                    0.0
                });
                channel.overlap.push_back(0.0);
                channel.overlap_weight.push_back(0.0);
            }
        }
    }
}

fn validate_playback_speed(playback_speed: f32) -> Result<()> {
    if !playback_speed.is_finite() || playback_speed <= 0.0 {
        bail!("playback speed must be finite and greater than zero");
    }
    Ok(())
}

fn canonical_playback_speed(playback_speed: f32) -> f32 {
    if (playback_speed - 1.0).abs() <= UNITY_SPEED_EPSILON {
        1.0
    } else {
        playback_speed
    }
}

/// Preserve the analysis-frame phase relationships around spectral peaks.
///
/// A conventional phase vocoder advances every FFT bin independently. That
/// smears a tone or transient across unrelated phases and can reduce its level.
/// Identity phase locking advances each local peak normally, while surrounding
/// bins retain their analysis-phase offset from that peak.
fn identity_phase_lock(
    magnitude: &[f32],
    analysis_phase: &[f32],
    propagated_phase: &[f32],
    synthesis_phase: &mut [f32],
    peaks: &mut Vec<usize>,
) {
    debug_assert_eq!(magnitude.len(), analysis_phase.len());
    debug_assert_eq!(magnitude.len(), propagated_phase.len());
    debug_assert_eq!(magnitude.len(), synthesis_phase.len());

    let bins = magnitude.len();
    if bins < 3 {
        synthesis_phase.copy_from_slice(propagated_phase);
        return;
    }

    peaks.clear();
    for bin in 1..bins - 1 {
        if magnitude[bin] > magnitude[bin - 1] && magnitude[bin] >= magnitude[bin + 1] {
            peaks.push(bin);
        }
    }
    if peaks.is_empty() {
        synthesis_phase.copy_from_slice(propagated_phase);
        return;
    }

    for (peak_index, &peak) in peaks.iter().enumerate() {
        let first = if peak_index == 0 {
            0
        } else {
            (peaks[peak_index - 1] + peak) / 2 + 1
        };
        let last = if peak_index + 1 == peaks.len() {
            bins - 1
        } else {
            (peak + peaks[peak_index + 1]) / 2
        };
        let peak_phase = propagated_phase[peak];
        for bin in first..=last {
            synthesis_phase[bin] =
                peak_phase + wrap_phase(analysis_phase[bin] - analysis_phase[peak]);
        }
    }

    // These self-conjugate bins must remain real for a real-valued inverse FFT.
    synthesis_phase[0] = analysis_phase[0];
    synthesis_phase[bins - 1] = analysis_phase[bins - 1];
}

fn speed_smoothing_coefficient(sample_rate: u32, analysis_hop: usize) -> f32 {
    let frame_duration_seconds = analysis_hop as f32 / sample_rate as f32;
    1.0 - (-frame_duration_seconds / SPEED_SMOOTHING_TIME_SECONDS).exp()
}

pub fn wrap_phase(phase: f32) -> f32 {
    (phase + PI).rem_euclid(TAU) - PI
}

#[cfg(test)]
mod tests {
    use std::f32::consts::{PI, TAU};

    use super::{
        PhaseVocoder, PhaseVocoderConfig, SPEED_SMOOTHING_TIME_SECONDS, UNITY_SPEED_EPSILON,
        speed_smoothing_coefficient, wrap_phase,
    };

    const SAMPLE_RATE: u32 = 48_000;
    const FFT_SIZE: usize = 2048;
    const ANALYSIS_HOP: usize = 512;

    fn sine_wave(frequency: f32, seconds: f32, channels: usize) -> Vec<f32> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut samples = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            let sample = (TAU * frequency * frame as f32 / SAMPLE_RATE as f32).sin() * 0.5;
            samples.extend(std::iter::repeat_n(sample, channels));
        }
        samples
    }

    fn multi_tone(seconds: f32) -> Vec<f32> {
        let frequencies = [173.0, 440.0, 997.0, 2_137.0, 6_103.0];
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..frames)
            .map(|frame| {
                frequencies
                    .iter()
                    .map(|frequency| {
                        (TAU * frequency * frame as f32 / SAMPLE_RATE as f32).sin() * 0.08
                    })
                    .sum()
            })
            .collect()
    }

    fn separated_attacks(channels: usize) -> Vec<f32> {
        let frames = SAMPLE_RATE as usize * 2;
        let attack_frames = [SAMPLE_RATE as usize / 2, SAMPLE_RATE as usize];
        let mut samples = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            let sample = attack_frames
                .iter()
                .filter_map(|attack| frame.checked_sub(*attack))
                .filter(|age| *age < SAMPLE_RATE as usize / 10)
                .map(|age| {
                    let envelope = (-(age as f32) / (SAMPLE_RATE as f32 * 0.015)).exp();
                    let tone = (TAU * 1_700.0 * age as f32 / SAMPLE_RATE as f32).sin();
                    envelope * tone * 0.6
                })
                .sum::<f32>();
            samples.extend(std::iter::repeat_n(sample, channels));
        }
        samples
    }

    fn process_at_speed(input: &[f32], channels: usize, speed: f32) -> Vec<f32> {
        let mut vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: FFT_SIZE,
            analysis_hop: ANALYSIS_HOP,
            channel_count: channels,
            playback_speed: speed,
        })
        .unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(4096 * channels) {
            vocoder.process(chunk, &mut output).unwrap();
        }
        vocoder.flush(&mut output).unwrap();
        output
    }

    fn frequency_from_positive_crossings(samples: &[f32]) -> f32 {
        let trim = FFT_SIZE * 2;
        let usable = &samples[trim.min(samples.len())..samples.len().saturating_sub(trim)];
        let crossings = usable
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count();
        crossings as f32 * SAMPLE_RATE as f32 / usable.len() as f32
    }

    fn assert_duration(input_frames: usize, output_frames: usize, speed: f32) {
        let expected = input_frames as f32 / speed;
        let tolerance = FFT_SIZE as f32 * 2.0;
        assert!(
            (output_frames as f32 - expected).abs() <= tolerance,
            "{output_frames} output frames differs from expected {expected}"
        );
    }

    #[test]
    fn phase_wrapping_uses_minus_pi_to_pi_interval() {
        assert!((wrap_phase(0.0) - 0.0).abs() < 1e-6);
        assert!((wrap_phase(PI) + PI).abs() < 1e-6);
        assert!((wrap_phase(-PI) + PI).abs() < 1e-6);
        assert!((wrap_phase(3.0 * PI) + PI).abs() < 1e-5);
        assert!((wrap_phase(-3.0 * PI) + PI).abs() < 1e-5);
        assert!((wrap_phase(8.0 * TAU + 0.25) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn speed_smoothing_depends_on_elapsed_time_not_hop_size() {
        fn remaining_distance_after(
            sample_rate: u32,
            analysis_hop: usize,
            frame_count: i32,
        ) -> f32 {
            let coefficient = speed_smoothing_coefficient(sample_rate, analysis_hop);
            (1.0 - coefficient).powi(frame_count)
        }

        // Both configurations advance exactly 100 ms of source audio.
        let coarse_hop_remaining = remaining_distance_after(48_000, 480, 10);
        let fine_hop_remaining = remaining_distance_after(48_000, 120, 40);
        let expected = (-0.1 / SPEED_SMOOTHING_TIME_SECONDS).exp();

        assert!((coarse_hop_remaining - fine_hop_remaining).abs() < 1e-6);
        assert!((coarse_hop_remaining - expected).abs() < 1e-6);
    }

    #[test]
    fn construction_accepts_a_non_default_configuration() {
        let vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: 44_100,
            fft_size: 1024,
            analysis_hop: 128,
            channel_count: 2,
            playback_speed: 0.8,
        })
        .unwrap();

        assert_eq!(vocoder.sample_rate(), 44_100);
        assert_eq!(vocoder.latency_frames(), 896);
    }

    #[test]
    fn unity_speed_preserves_duration_pitch_and_reasonable_gain() {
        let input = sine_wave(440.0, 2.0, 1);
        let output = process_at_speed(&input, 1, 1.0);
        assert!(!output.is_empty());
        assert_duration(input.len(), output.len(), 1.0);
        assert!((frequency_from_positive_crossings(&output) - 440.0).abs() < 4.0);
        assert!(output.iter().all(|sample| sample.is_finite()));
        let peak = output
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert!((0.25..=0.8).contains(&peak), "unexpected peak {peak}");
    }

    #[test]
    fn unity_speed_reconstructs_the_steady_state_waveform() {
        let input = multi_tone(4.0);
        let output = process_at_speed(&input, 1, 1.0);
        let first = FFT_SIZE * 2;
        let last = input.len() - FFT_SIZE * 2;
        let maximum_error = input[first..last]
            .iter()
            .zip(&output[first..last])
            .map(|(input, output)| (input - output).abs())
            .fold(0.0_f32, f32::max);

        assert!(
            maximum_error < 1e-4,
            "unity reconstruction error is {maximum_error}"
        );
    }

    #[test]
    fn near_unity_speed_is_canonicalized_to_transparent_unity() {
        let input = multi_tone(1.0);
        let output = process_at_speed(&input, 1, 1.0 + UNITY_SPEED_EPSILON * 0.5);
        let first = FFT_SIZE * 2;
        let last = input.len() - FFT_SIZE * 2;
        let maximum_error = input[first..last]
            .iter()
            .zip(&output[first..last])
            .map(|(input, output)| (input - output).abs())
            .fold(0.0_f32, f32::max);

        assert!(
            maximum_error < 1e-4,
            "near-unity reconstruction error is {maximum_error}"
        );
    }

    #[test]
    fn speed_changes_do_not_collapse_the_signal_level() {
        let input = multi_tone(6.0);
        let mut vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: FFT_SIZE,
            analysis_hop: ANALYSIS_HOP,
            channel_count: 1,
            playback_speed: 0.75,
        })
        .unwrap();
        let mut output = Vec::new();
        for (chunk_index, chunk) in input.chunks(1024).enumerate() {
            if chunk_index == 94 {
                vocoder.set_playback_speed(1.35).unwrap();
            } else if chunk_index == 188 {
                vocoder.set_playback_speed(0.55).unwrap();
            }
            vocoder.process(chunk, &mut output).unwrap();
        }
        vocoder.flush(&mut output).unwrap();

        let window = SAMPLE_RATE as usize / 50;
        let trim = FFT_SIZE * 2;
        let rms = output[trim..output.len() - trim]
            .chunks_exact(window)
            .map(|samples| {
                (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32)
                    .sqrt()
            })
            .collect::<Vec<_>>();
        let minimum = rms.iter().copied().fold(f32::INFINITY, f32::min);
        let mean = rms.iter().sum::<f32>() / rms.len() as f32;

        assert!(
            minimum > mean * 0.35,
            "short-window RMS collapsed to {minimum} with mean {mean}"
        );
    }

    #[test]
    fn separated_attacks_trigger_coherent_phase_resets() {
        let input = separated_attacks(2);
        let mut vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: FFT_SIZE,
            analysis_hop: ANALYSIS_HOP,
            channel_count: 2,
            playback_speed: 0.6,
        })
        .unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(1024) {
            vocoder.process(chunk, &mut output).unwrap();
        }
        vocoder.flush(&mut output).unwrap();

        assert!(
            (2..=4).contains(&vocoder.detected_transients),
            "expected two attacks without rapid retriggering, detected {}",
            vocoder.detected_transients
        );
        assert!(output.chunks_exact(2).all(|frame| frame[0] == frame[1]));
    }

    #[test]
    fn steady_tone_does_not_repeatedly_trigger_transient_resets() {
        let input = sine_wave(440.0, 2.0, 1);
        let mut vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: FFT_SIZE,
            analysis_hop: ANALYSIS_HOP,
            channel_count: 1,
            playback_speed: 0.6,
        })
        .unwrap();
        let mut output = Vec::new();
        vocoder.process(&input, &mut output).unwrap();
        vocoder.flush(&mut output).unwrap();

        assert!(
            vocoder.detected_transients <= 1,
            "steady tone caused {} transient resets",
            vocoder.detected_transients
        );
    }

    #[test]
    fn external_transient_timestamps_drive_phase_resets() {
        let input = vec![0.0; SAMPLE_RATE as usize / 10];
        let mut vocoder = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: 256,
            analysis_hop: 64,
            channel_count: 1,
            playback_speed: 0.6,
        })
        .unwrap();
        vocoder.use_external_transient_timeline();
        let mut output = Vec::new();
        vocoder
            .process_with_transients(&input, &[500, 2_000], &mut output)
            .unwrap();
        vocoder.flush(&mut output).unwrap();

        assert_eq!(vocoder.detected_transients(), 2);
        assert!(vocoder.pending_external_transients.is_empty());
    }

    #[test]
    fn slower_speed_preserves_pitch_and_doubles_duration() {
        let input = sine_wave(440.0, 2.0, 1);
        let output = process_at_speed(&input, 1, 0.5);
        assert_duration(input.len(), output.len(), 0.5);
        assert!((frequency_from_positive_crossings(&output) - 440.0).abs() < 5.0);
    }

    #[test]
    fn faster_speed_preserves_pitch_and_shortens_duration() {
        let input = sine_wave(440.0, 2.0, 1);
        let output = process_at_speed(&input, 1, 1.5);
        assert_duration(input.len(), output.len(), 1.5);
        assert!((frequency_from_positive_crossings(&output) - 440.0).abs() < 5.0);
    }

    #[test]
    fn stereo_channels_remain_independent_and_interleaved() {
        let frames = SAMPLE_RATE as usize * 2;
        let mut input = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            input.push((TAU * 440.0 * frame as f32 / SAMPLE_RATE as f32).sin() * 0.5);
            input.push((TAU * 660.0 * frame as f32 / SAMPLE_RATE as f32).sin() * 0.5);
        }
        let output = process_at_speed(&input, 2, 0.75);
        assert_eq!(output.len() % 2, 0);
        let left = output.iter().step_by(2).copied().collect::<Vec<_>>();
        let right = output
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<_>>();
        assert!((frequency_from_positive_crossings(&left) - 440.0).abs() < 5.0);
        assert!((frequency_from_positive_crossings(&right) - 660.0).abs() < 5.0);
    }

    #[test]
    fn reset_matches_a_fresh_processor() {
        let input = sine_wave(440.0, 0.25, 1);
        let mut reused = PhaseVocoder::new(PhaseVocoderConfig {
            sample_rate: SAMPLE_RATE,
            fft_size: FFT_SIZE,
            analysis_hop: ANALYSIS_HOP,
            channel_count: 1,
            playback_speed: 1.0,
        })
        .unwrap();
        let mut discarded = Vec::new();
        reused.process(&input, &mut discarded).unwrap();
        reused.reset();
        let mut reused_output = Vec::new();
        reused.process(&input, &mut reused_output).unwrap();
        reused.flush(&mut reused_output).unwrap();

        let fresh_output = process_at_speed(&input, 1, 1.0);
        assert_eq!(reused_output, fresh_output);
    }
}
