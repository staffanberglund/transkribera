use std::collections::VecDeque;

use anyhow::{Result, bail};

use super::{
    filter_bank::{BAND_COUNT, FiveBandFilterBank, FiveBandFilterBankConfig},
    onset_detector::{OnsetDetector, OnsetDetectorConfig},
    phase_vocoder::{PhaseVocoder, PhaseVocoderConfig},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BandAnalysisConfig {
    pub fft_size: usize,
    pub analysis_hop: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FiveBandTempoProcessorConfig {
    pub sample_rate: u32,
    pub channel_count: usize,
    pub playback_speed: f32,
    pub crossover_hz: [f32; BAND_COUNT - 1],
    pub filter_tap_count: usize,
    pub band_analysis: [BandAnalysisConfig; BAND_COUNT],
}

/// A full-rate five-band tempo processor.
///
/// Each band may use a different FFT size and analysis hop. Persistent output
/// queues absorb the different block-production schedules: streaming emits
/// only samples available from every band, while flush pads the shorter DSP
/// tails with silence so all queued samples are drained.
pub struct FiveBandTempoProcessor {
    sample_rate: u32,
    channel_count: usize,
    filter_bank: FiveBandFilterBank,
    onset_detector: OnsetDetector,
    phase_vocoders: Vec<PhaseVocoder>,
    transient_events: Vec<u64>,
    band_input: [Vec<f32>; BAND_COUNT],
    processor_output: Vec<f32>,
    output_queues: [VecDeque<f32>; BAND_COUNT],
    latency_frames: usize,
    has_pending_audio: bool,
}

impl FiveBandTempoProcessor {
    pub fn new(config: FiveBandTempoProcessorConfig) -> Result<Self> {
        let filter_bank = FiveBandFilterBank::new(FiveBandFilterBankConfig {
            sample_rate: config.sample_rate,
            channel_count: config.channel_count,
            crossover_hz: config.crossover_hz,
            tap_count: config.filter_tap_count,
        })?;
        let detector_fft_size = config
            .band_analysis
            .iter()
            .map(|analysis| analysis.fft_size)
            .min()
            .unwrap_or(512)
            .min(512);
        let detector_analysis_hop = (detector_fft_size / 8).max(1);
        let onset_detector = OnsetDetector::new(OnsetDetectorConfig {
            sample_rate: config.sample_rate,
            fft_size: detector_fft_size,
            analysis_hop: detector_analysis_hop,
            channel_count: config.channel_count,
        })?;
        let output_origin_frames = config
            .band_analysis
            .iter()
            .map(|analysis| analysis.fft_size / 2)
            .max()
            .unwrap_or(0);
        let phase_vocoders = config
            .band_analysis
            .into_iter()
            .map(|analysis| {
                let mut processor = PhaseVocoder::new(PhaseVocoderConfig {
                    sample_rate: config.sample_rate,
                    fft_size: analysis.fft_size,
                    analysis_hop: analysis.analysis_hop,
                    channel_count: config.channel_count,
                    playback_speed: config.playback_speed,
                })?;
                processor.use_external_tempo_timeline(output_origin_frames);
                Ok(processor)
            })
            .collect::<Result<Vec<_>>>()?;
        let phase_latency = phase_vocoders
            .iter()
            .map(PhaseVocoder::latency_frames)
            .max()
            .unwrap_or(0);
        let latency_frames = filter_bank.latency_frames() + phase_latency;

        Ok(Self {
            sample_rate: config.sample_rate,
            channel_count: config.channel_count,
            filter_bank,
            onset_detector,
            phase_vocoders,
            transient_events: Vec::new(),
            band_input: std::array::from_fn(|_| Vec::new()),
            processor_output: Vec::new(),
            output_queues: std::array::from_fn(|_| VecDeque::new()),
            latency_frames,
            has_pending_audio: false,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    pub fn set_playback_speed(&mut self, playback_speed: f32) -> Result<()> {
        for processor in &mut self.phase_vocoders {
            processor.set_playback_speed(playback_speed)?;
        }
        Ok(())
    }

    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()> {
        self.transient_events.clear();
        self.onset_detector
            .process(input, &mut self.transient_events)?;
        let filter_delay = self.filter_bank.latency_frames() as u64;
        for timestamp in &mut self.transient_events {
            *timestamp = timestamp.saturating_add(filter_delay);
        }
        for band in &mut self.band_input {
            band.clear();
        }
        self.filter_bank.process(input, &mut self.band_input)?;
        self.process_band_input(false)?;
        self.drain_aligned_output(output, false)?;
        self.has_pending_audio |= !input.is_empty();
        Ok(())
    }

    pub fn flush(&mut self, output: &mut Vec<f32>) -> Result<()> {
        if !self.has_pending_audio {
            return Ok(());
        }
        self.transient_events.clear();
        for band in &mut self.band_input {
            band.clear();
        }
        self.filter_bank.flush(&mut self.band_input);
        self.process_band_input(true)?;
        self.drain_aligned_output(output, true)?;
        self.has_pending_audio = false;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.filter_bank.reset();
        self.onset_detector.reset();
        for processor in &mut self.phase_vocoders {
            processor.reset();
        }
        for band in &mut self.band_input {
            band.clear();
        }
        self.processor_output.clear();
        self.transient_events.clear();
        for queue in &mut self.output_queues {
            queue.clear();
        }
        self.has_pending_audio = false;
    }

    fn process_band_input(&mut self, flush: bool) -> Result<()> {
        for ((processor, input), queue) in self
            .phase_vocoders
            .iter_mut()
            .zip(&self.band_input)
            .zip(&mut self.output_queues)
        {
            self.processor_output.clear();
            processor.process_with_transients(
                input,
                &self.transient_events,
                &mut self.processor_output,
            )?;
            if flush {
                processor.flush(&mut self.processor_output)?;
            }
            queue.extend(self.processor_output.drain(..));
        }
        Ok(())
    }

    fn drain_aligned_output(&mut self, output: &mut Vec<f32>, flushing: bool) -> Result<()> {
        let sample_count = if flushing {
            self.output_queues
                .iter()
                .map(VecDeque::len)
                .max()
                .unwrap_or(0)
        } else {
            self.output_queues
                .iter()
                .map(VecDeque::len)
                .min()
                .unwrap_or(0)
        };
        if !sample_count.is_multiple_of(self.channel_count) {
            let lengths = self.output_queues.each_ref().map(VecDeque::len);
            bail!("multiband output lost channel alignment: {lengths:?}");
        }

        output.reserve(sample_count);
        for _ in 0..sample_count {
            let sample = self
                .output_queues
                .iter_mut()
                .map(|queue| queue.pop_front().unwrap_or(0.0))
                .sum();
            output.push(sample);
        }
        debug_assert!(!flushing || self.output_queues.iter().all(VecDeque::is_empty));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use crate::dsp::phase_vocoder::{PhaseVocoder, PhaseVocoderConfig};

    use super::{
        BAND_COUNT, BandAnalysisConfig, FiveBandTempoProcessor, FiveBandTempoProcessorConfig,
    };

    const SAMPLE_RATE: u32 = 8_000;
    const FFT_SIZE: usize = 256;
    const ANALYSIS_HOP: usize = 64;

    fn config(channel_count: usize, playback_speed: f32) -> FiveBandTempoProcessorConfig {
        FiveBandTempoProcessorConfig {
            sample_rate: SAMPLE_RATE,
            channel_count,
            playback_speed,
            crossover_hz: [300.0, 800.0, 1_600.0, 2_800.0],
            filter_tap_count: 33,
            band_analysis: [BandAnalysisConfig {
                fft_size: FFT_SIZE,
                analysis_hop: ANALYSIS_HOP,
            }; BAND_COUNT],
        }
    }

    fn sine_wave(frequency: f32, seconds: f32, channel_count: usize) -> Vec<f32> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut input = Vec::with_capacity(frames * channel_count);
        for frame in 0..frames {
            let sample = (TAU * frequency * frame as f32 / SAMPLE_RATE as f32).sin() * 0.25;
            input.extend(std::iter::repeat_n(sample, channel_count));
        }
        input
    }

    fn process_at_speed(speed: f32, channel_count: usize) -> (usize, usize, Vec<f32>) {
        let input = sine_wave(440.0, 0.5, channel_count);
        let input_frames = input.len() / channel_count;
        let mut processor = FiveBandTempoProcessor::new(config(channel_count, speed)).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(317 * channel_count) {
            processor.process(chunk, &mut output).unwrap();
        }
        let samples_before_flush = output.len();
        processor.flush(&mut output).unwrap();
        (input_frames, samples_before_flush, output)
    }

    #[test]
    fn constructs_exactly_five_phase_vocoders() {
        let processor = FiveBandTempoProcessor::new(config(1, 1.0)).unwrap();
        assert_eq!(processor.phase_vocoders.len(), BAND_COUNT);
    }

    #[test]
    fn slow_unity_and_fast_speeds_have_expected_duration() {
        for speed in [0.5, 1.0, 1.5] {
            let (input_frames, samples_before_flush, output) = process_at_speed(speed, 1);
            let expected_frames = input_frames as f32 / speed;
            let tolerance = (FFT_SIZE * 3) as f32;
            assert!(
                (output.len() as f32 - expected_frames).abs() < tolerance,
                "{} frames at {speed}x differs from expected {expected_frames}",
                output.len()
            );
            assert!(output.iter().all(|sample| sample.is_finite()));
            let (peak_index, peak) = output
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
                .map(|(index, sample)| (index, sample.abs()))
                .unwrap_or((0, 0.0));
            assert!(
                (0.01..2.0).contains(&peak),
                "unexpected peak {peak} at sample {peak_index} ({samples_before_flush} before flush) at {speed}x"
            );
        }
    }

    #[test]
    fn stereo_output_remains_complete_and_finite() {
        let (_, _, output) = process_at_speed(0.75, 2);
        assert_eq!(output.len() % 2, 0);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.chunks_exact(2).any(|frame| frame[0] != 0.0));
        assert!(output.chunks_exact(2).all(|frame| frame[0] == frame[1]));
    }

    #[test]
    fn repeated_speed_changes_do_not_diverge_band_lengths() {
        let input = sine_wave(440.0, 1.0, 1);
        let mut processor = FiveBandTempoProcessor::new(config(1, 1.0)).unwrap();
        let mut output = Vec::new();
        for (index, chunk) in input.chunks(211).enumerate() {
            let speed = [0.5, 1.25, 0.75, 1.5][index % 4];
            processor.set_playback_speed(speed).unwrap();
            processor.process(chunk, &mut output).unwrap();
        }
        processor.flush(&mut output).unwrap();
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(processor.output_queues.iter().all(|queue| queue.is_empty()));
    }

    #[test]
    fn reset_matches_a_fresh_processor() {
        let input = sine_wave(440.0, 0.25, 1);
        let mut reused = FiveBandTempoProcessor::new(config(1, 0.75)).unwrap();
        let mut discarded = Vec::new();
        reused.process(&input, &mut discarded).unwrap();
        reused.reset();
        let mut reused_output = Vec::new();
        reused.process(&input, &mut reused_output).unwrap();
        reused.flush(&mut reused_output).unwrap();

        let mut fresh = FiveBandTempoProcessor::new(config(1, 0.75)).unwrap();
        let mut fresh_output = Vec::new();
        fresh.process(&input, &mut fresh_output).unwrap();
        fresh.flush(&mut fresh_output).unwrap();
        assert_eq!(reused_output, fresh_output);
    }

    #[test]
    fn flushing_twice_does_not_append_another_tail() {
        let input = sine_wave(440.0, 0.25, 1);
        let mut processor = FiveBandTempoProcessor::new(config(1, 0.75)).unwrap();
        let mut output = Vec::new();
        processor.process(&input, &mut output).unwrap();
        processor.flush(&mut output).unwrap();
        let length_after_first_flush = output.len();
        processor.flush(&mut output).unwrap();
        assert_eq!(output.len(), length_after_first_flush);
    }

    #[test]
    fn different_band_resolutions_remain_aligned_through_speed_changes() {
        let mut heterogeneous = config(2, 1.0);
        heterogeneous.band_analysis = [
            BandAnalysisConfig {
                fft_size: 512,
                analysis_hop: 64,
            },
            BandAnalysisConfig {
                fft_size: 256,
                analysis_hop: 32,
            },
            BandAnalysisConfig {
                fft_size: 128,
                analysis_hop: 16,
            },
            BandAnalysisConfig {
                fft_size: 64,
                analysis_hop: 8,
            },
            BandAnalysisConfig {
                fft_size: 32,
                analysis_hop: 4,
            },
        ];
        let mut processor = FiveBandTempoProcessor::new(heterogeneous).unwrap();
        assert_eq!(processor.latency_frames(), 16 + 512 - 64);

        let input = sine_wave(440.0, 1.0, 2);
        let mut output = Vec::new();
        for (index, chunk) in input.chunks(422).enumerate() {
            processor
                .set_playback_speed([0.5, 1.25, 0.75, 1.5][index % 4])
                .unwrap();
            processor.process(chunk, &mut output).unwrap();
        }
        processor.flush(&mut output).unwrap();

        assert_eq!(output.len() % 2, 0);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(processor.output_queues.iter().all(|queue| queue.is_empty()));
    }

    #[test]
    fn different_resolutions_apply_the_same_shared_onsets() {
        let mut heterogeneous = config(2, 0.6);
        heterogeneous.band_analysis = [
            BandAnalysisConfig {
                fft_size: 512,
                analysis_hop: 64,
            },
            BandAnalysisConfig {
                fft_size: 256,
                analysis_hop: 32,
            },
            BandAnalysisConfig {
                fft_size: 128,
                analysis_hop: 16,
            },
            BandAnalysisConfig {
                fft_size: 64,
                analysis_hop: 8,
            },
            BandAnalysisConfig {
                fft_size: 32,
                analysis_hop: 4,
            },
        ];
        let mut input = vec![0.0; SAMPLE_RATE as usize * 2];
        for frame in [2_000, 5_000] {
            for age in 0_usize..64 {
                let sample = if age.is_multiple_of(2) { 0.8 } else { -0.8 };
                input[(frame + age) * 2] = sample;
                input[(frame + age) * 2 + 1] = sample;
            }
        }
        let mut processor = FiveBandTempoProcessor::new(heterogeneous).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(146) {
            processor.process(chunk, &mut output).unwrap();
        }
        processor.flush(&mut output).unwrap();
        let detections = processor
            .phase_vocoders
            .iter()
            .map(|vocoder| vocoder.detected_transients())
            .collect::<Vec<_>>();

        assert!(detections[0] >= 2, "shared detections: {detections:?}");
        assert!(detections.iter().all(|count| *count == detections[0]));
    }

    #[test]
    fn shared_tempo_timeline_aligns_impulses_across_resolutions() {
        let analyses = [
            BandAnalysisConfig {
                fft_size: 512,
                analysis_hop: 64,
            },
            BandAnalysisConfig {
                fft_size: 256,
                analysis_hop: 32,
            },
            BandAnalysisConfig {
                fft_size: 128,
                analysis_hop: 16,
            },
            BandAnalysisConfig {
                fft_size: 64,
                analysis_hop: 8,
            },
            BandAnalysisConfig {
                fft_size: 32,
                analysis_hop: 4,
            },
        ];
        for speed in [0.5, 0.75, 1.0, 1.5] {
            let positions = impulse_positions(analyses, speed, None);
            let spread = positions.iter().max().unwrap() - positions.iter().min().unwrap();
            assert!(spread <= 16, "{speed}x impulse positions: {positions:?}");
        }

        let positions = impulse_positions(analyses, 1.0, Some((1_000, 0.5)));
        let spread = positions.iter().max().unwrap() - positions.iter().min().unwrap();
        assert!(
            spread <= 16,
            "speed-transition impulse positions: {positions:?}"
        );
    }

    fn impulse_positions(
        analyses: [BandAnalysisConfig; BAND_COUNT],
        initial_speed: f32,
        speed_change: Option<(usize, f32)>,
    ) -> Vec<usize> {
        let input_frames = 8_000;
        let impulse_frame = 2_000_u64;
        analyses
            .into_iter()
            .map(|analysis| {
                let mut processor = PhaseVocoder::new(PhaseVocoderConfig {
                    sample_rate: SAMPLE_RATE,
                    fft_size: analysis.fft_size,
                    analysis_hop: analysis.analysis_hop,
                    channel_count: 1,
                    playback_speed: initial_speed,
                })
                .unwrap();
                processor.use_external_tempo_timeline(256);
                let mut input = vec![0.0; input_frames];
                input[impulse_frame as usize] = 1.0;
                let mut output = Vec::new();
                if let Some((change_frame, speed)) = speed_change {
                    processor
                        .process_with_transients(&input[..change_frame], &[], &mut output)
                        .unwrap();
                    processor.set_playback_speed(speed).unwrap();
                    processor
                        .process_with_transients(
                            &input[change_frame..],
                            &[impulse_frame],
                            &mut output,
                        )
                        .unwrap();
                } else {
                    processor
                        .process_with_transients(&input, &[impulse_frame], &mut output)
                        .unwrap();
                }
                processor.flush(&mut output).unwrap();
                output
                    .iter()
                    .enumerate()
                    .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
                    .map(|(index, _)| index)
                    .unwrap()
            })
            .collect()
    }
}
