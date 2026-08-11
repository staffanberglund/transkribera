use std::{collections::VecDeque, f64::consts::PI};

use anyhow::{Result, bail};

pub const BAND_COUNT: usize = 5;
pub const CROSSOVER_COUNT: usize = BAND_COUNT - 1;

/// Configuration for a five-band, linear-phase complementary FIR filter bank.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FiveBandFilterBankConfig {
    pub sample_rate: u32,
    pub channel_count: usize,
    pub crossover_hz: [f32; CROSSOVER_COUNT],
    pub tap_count: usize,
}

/// Splits interleaved PCM into five full-sample-rate bands.
///
/// Four cumulative low-pass signals (`L1` through `L4`) form the bands
/// `L1`, `L2-L1`, `L3-L2`, `L4-L3`, and `delayed_input-L4`. Consequently the
/// five bands sum to the input delayed by the common FIR group delay.
pub struct FiveBandFilterBank {
    channel_count: usize,
    tap_count: usize,
    group_delay_frames: usize,
    lowpass_coefficients: [Vec<f32>; CROSSOVER_COUNT],
    channel_history: Vec<VecDeque<f32>>,
    pending_tail_frames: usize,
}

impl FiveBandFilterBank {
    pub fn new(config: FiveBandFilterBankConfig) -> Result<Self> {
        validate_config(config)?;

        let lowpass_coefficients = config
            .crossover_hz
            .map(|cutoff_hz| design_lowpass(config.sample_rate, cutoff_hz, config.tap_count));
        let channel_history = (0..config.channel_count)
            .map(|_| VecDeque::from(vec![0.0; config.tap_count]))
            .collect();

        Ok(Self {
            channel_count: config.channel_count,
            tap_count: config.tap_count,
            group_delay_frames: (config.tap_count - 1) / 2,
            lowpass_coefficients,
            channel_history,
            pending_tail_frames: 0,
        })
    }

    pub fn latency_frames(&self) -> usize {
        self.group_delay_frames
    }

    pub fn process(&mut self, input: &[f32], output: &mut [Vec<f32>; BAND_COUNT]) -> Result<()> {
        if !input.len().is_multiple_of(self.channel_count) {
            bail!("interleaved filter-bank input does not contain complete audio frames");
        }

        let frame_count = input.len() / self.channel_count;
        reserve_output(output, input.len());
        for frame in input.chunks_exact(self.channel_count) {
            self.process_frame(frame, output);
        }
        if frame_count > 0 {
            self.pending_tail_frames = self.tap_count - 1;
        }
        Ok(())
    }

    /// Emits the complete FIR tail for every band.
    pub fn flush(&mut self, output: &mut [Vec<f32>; BAND_COUNT]) {
        let tail_frames = self.pending_tail_frames;
        reserve_output(output, tail_frames.saturating_mul(self.channel_count));
        let silent_frame = vec![0.0; self.channel_count];
        for _ in 0..tail_frames {
            self.process_frame(&silent_frame, output);
        }
        self.pending_tail_frames = 0;
    }

    pub fn reset(&mut self) {
        for history in &mut self.channel_history {
            for sample in history {
                *sample = 0.0;
            }
        }
        self.pending_tail_frames = 0;
    }

    fn process_frame(&mut self, frame: &[f32], output: &mut [Vec<f32>; BAND_COUNT]) {
        for (history, sample) in self.channel_history.iter_mut().zip(frame) {
            history.pop_back();
            history.push_front(*sample);

            let lowpass = self.lowpass_coefficients.each_ref().map(|coefficients| {
                coefficients
                    .iter()
                    .zip(history.iter())
                    .map(|(coefficient, sample)| coefficient * sample)
                    .sum::<f32>()
            });
            let delayed_input = history[self.group_delay_frames];
            let bands = [
                lowpass[0],
                lowpass[1] - lowpass[0],
                lowpass[2] - lowpass[1],
                lowpass[3] - lowpass[2],
                delayed_input - lowpass[3],
            ];
            for (band_output, sample) in output.iter_mut().zip(bands) {
                band_output.push(sample);
            }
        }
    }
}

fn reserve_output(output: &mut [Vec<f32>; BAND_COUNT], additional_samples: usize) {
    for band in output {
        band.reserve(additional_samples);
    }
}

fn validate_config(config: FiveBandFilterBankConfig) -> Result<()> {
    if config.sample_rate == 0 {
        bail!("filter-bank sample rate must be greater than zero");
    }
    if config.channel_count == 0 {
        bail!("filter-bank channel count must be greater than zero");
    }
    if config.tap_count < 3 || config.tap_count.is_multiple_of(2) {
        bail!("filter-bank tap count must be an odd number of at least three");
    }

    let nyquist_hz = config.sample_rate as f32 / 2.0;
    let mut previous_hz = 0.0;
    for crossover_hz in config.crossover_hz {
        if !crossover_hz.is_finite() || crossover_hz <= previous_hz || crossover_hz >= nyquist_hz {
            bail!("filter-bank crossovers must be finite, strictly increasing, and below Nyquist");
        }
        previous_hz = crossover_hz;
    }
    Ok(())
}

fn design_lowpass(sample_rate: u32, cutoff_hz: f32, tap_count: usize) -> Vec<f32> {
    let cutoff_cycles_per_sample = cutoff_hz as f64 / sample_rate as f64;
    let center = (tap_count - 1) as f64 / 2.0;
    let mut coefficients = (0..tap_count)
        .map(|index| {
            let offset = index as f64 - center;
            let sinc = if offset == 0.0 {
                2.0 * cutoff_cycles_per_sample
            } else {
                (2.0 * PI * cutoff_cycles_per_sample * offset).sin() / (PI * offset)
            };
            let position = index as f64 / (tap_count - 1) as f64;
            let blackman =
                0.42 - 0.5 * (2.0 * PI * position).cos() + 0.08 * (4.0 * PI * position).cos();
            sinc * blackman
        })
        .collect::<Vec<_>>();
    let dc_gain = coefficients.iter().sum::<f64>();
    for coefficient in &mut coefficients {
        *coefficient /= dc_gain;
    }
    coefficients
        .into_iter()
        .map(|coefficient| coefficient as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{BAND_COUNT, FiveBandFilterBank, FiveBandFilterBankConfig};

    const SAMPLE_RATE: u32 = 48_000;
    const TAP_COUNT: usize = 257;
    const CROSSOVERS: [f32; 4] = [1_000.0, 3_000.0, 6_000.0, 10_000.0];

    fn config(channel_count: usize) -> FiveBandFilterBankConfig {
        FiveBandFilterBankConfig {
            sample_rate: SAMPLE_RATE,
            channel_count,
            crossover_hz: CROSSOVERS,
            tap_count: TAP_COUNT,
        }
    }

    fn empty_output() -> [Vec<f32>; BAND_COUNT] {
        std::array::from_fn(|_| Vec::new())
    }

    fn recombine(output: &[Vec<f32>; BAND_COUNT]) -> Vec<f32> {
        (0..output[0].len())
            .map(|index| output.iter().map(|band| band[index]).sum())
            .collect()
    }

    fn expected_reconstruction(input: &[f32], channel_count: usize) -> Vec<f32> {
        let delay_samples = ((TAP_COUNT - 1) / 2) * channel_count;
        let tail_samples = (TAP_COUNT - 1) * channel_count;
        let mut expected = vec![0.0; input.len() + tail_samples];
        expected[delay_samples..delay_samples + input.len()].copy_from_slice(input);
        expected
    }

    fn assert_samples_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        let maximum_error = actual
            .iter()
            .zip(expected)
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f32, f32::max);
        assert!(maximum_error < 2e-6, "maximum error was {maximum_error}");
    }

    #[test]
    fn impulse_recombines_to_a_delayed_impulse() {
        let mut bank = FiveBandFilterBank::new(config(1)).unwrap();
        let input = [1.0];
        let mut output = empty_output();

        bank.process(&input, &mut output).unwrap();
        bank.flush(&mut output);

        assert_eq!(bank.latency_frames(), (TAP_COUNT - 1) / 2);
        assert_samples_close(&recombine(&output), &expected_reconstruction(&input, 1));
    }

    #[test]
    fn arbitrary_chunks_reconstruct_interleaved_stereo_without_drift() {
        let frame_count = 997;
        let channel_count = 2;
        let mut random_state = 0x1234_5678_u32;
        let input = (0..frame_count * channel_count)
            .map(|_| {
                random_state = random_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                ((random_state >> 8) as f32 / 0x00ff_ffff_u32 as f32) * 2.0 - 1.0
            })
            .collect::<Vec<_>>();
        let mut bank = FiveBandFilterBank::new(config(channel_count)).unwrap();
        let mut output = empty_output();
        let chunk_frames = [1, 7, 64, 3, 129, 11];
        let mut input_frame = 0;
        let mut chunk_index = 0;
        while input_frame < frame_count {
            let frames =
                chunk_frames[chunk_index % chunk_frames.len()].min(frame_count - input_frame);
            let start = input_frame * channel_count;
            let end = (input_frame + frames) * channel_count;
            bank.process(&input[start..end], &mut output).unwrap();
            input_frame += frames;
            chunk_index += 1;
        }
        bank.flush(&mut output);

        assert_samples_close(
            &recombine(&output),
            &expected_reconstruction(&input, channel_count),
        );
        assert!(output.iter().all(|band| band.len() == output[0].len()));
    }

    #[test]
    fn reset_matches_a_fresh_filter_bank() {
        let input = (0..512)
            .map(|frame| (TAU * 2_000.0 * frame as f32 / SAMPLE_RATE as f32).sin())
            .collect::<Vec<_>>();
        let mut reused = FiveBandFilterBank::new(config(1)).unwrap();
        let mut discarded = empty_output();
        reused.process(&input, &mut discarded).unwrap();
        reused.reset();
        let mut reused_output = empty_output();
        reused.process(&input, &mut reused_output).unwrap();
        reused.flush(&mut reused_output);

        let mut fresh = FiveBandFilterBank::new(config(1)).unwrap();
        let mut fresh_output = empty_output();
        fresh.process(&input, &mut fresh_output).unwrap();
        fresh.flush(&mut fresh_output);
        assert_eq!(reused_output, fresh_output);
    }

    #[test]
    fn separated_tones_are_strongest_in_the_expected_bands() {
        let frequencies = [200.0, 2_000.0, 4_500.0, 8_000.0, 15_000.0];
        for (expected_band, frequency) in frequencies.into_iter().enumerate() {
            let input = (0..4096)
                .map(|frame| (TAU * frequency * frame as f32 / SAMPLE_RATE as f32).sin())
                .collect::<Vec<_>>();
            let mut bank = FiveBandFilterBank::new(config(1)).unwrap();
            let mut output = empty_output();
            bank.process(&input, &mut output).unwrap();

            let rms = output.map(|band| {
                let steady = &band[TAP_COUNT..];
                (steady.iter().map(|sample| sample * sample).sum::<f32>() / steady.len() as f32)
                    .sqrt()
            });
            let strongest_other = rms
                .iter()
                .enumerate()
                .filter(|(band, _)| *band != expected_band)
                .map(|(_, rms)| *rms)
                .fold(0.0_f32, f32::max);
            assert!(
                rms[expected_band] > strongest_other * 5.0,
                "{frequency} Hz energy was distributed as {rms:?}"
            );
        }
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let mut invalid = config(0);
        assert!(FiveBandFilterBank::new(invalid).is_err());
        invalid = config(1);
        invalid.tap_count = 256;
        assert!(FiveBandFilterBank::new(invalid).is_err());
        invalid = config(1);
        invalid.crossover_hz = [1_000.0, 6_000.0, 3_000.0, 10_000.0];
        assert!(FiveBandFilterBank::new(invalid).is_err());
        invalid = config(1);
        invalid.crossover_hz[3] = SAMPLE_RATE as f32 / 2.0;
        assert!(FiveBandFilterBank::new(invalid).is_err());
    }
}
