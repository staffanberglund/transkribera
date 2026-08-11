use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::json::{
    object_array, optional_unsigned_integer, unsigned_integer, unsigned_integer_array,
};

use super::{
    filter_bank::BAND_COUNT,
    multiband::{BandAnalysisConfig, FiveBandTempoProcessor, FiveBandTempoProcessorConfig},
    phase_vocoder::{PhaseVocoder, PhaseVocoderConfig},
};

pub const MIN_PLAYBACK_SPEED: f32 = 0.25;
pub const MAX_PLAYBACK_SPEED: f32 = 1.50;

const DSP_CONFIG_VERSION: u64 = 1;
const DSP_CONFIG_DIRECTORY: &str = "transcription-mvp";
const DSP_CONFIG_FILENAME: &str = "dsp.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AnalysisResolution {
    fft_size: usize,
    analysis_hop: usize,
}

const DEFAULT_ANALYSIS_RESOLUTIONS: &[AnalysisResolution] = &[AnalysisResolution {
    fft_size: 2048,
    analysis_hop: 512,
}];

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProcessorLayout {
    SingleBand(AnalysisResolution),
    FiveBand {
        analysis: [AnalysisResolution; BAND_COUNT],
        crossover_hz: [u64; BAND_COUNT - 1],
        filter_tap_count: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoProcessorConfig {
    pub sample_rate: u32,
    pub channel_count: usize,
    pub playback_speed: f32,
}

/// A GStreamer-independent tempo-processing pipeline.
///
/// The pipeline may contain one phase vocoder, several phase vocoders, or a
/// different implementation. Callers deliberately cannot depend on that
/// internal topology.
pub trait TempoProcessor: Send {
    fn sample_rate(&self) -> u32;
    fn latency_frames(&self) -> usize;
    fn set_playback_speed(&mut self, playback_speed: f32) -> Result<()>;
    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()>;
    fn flush(&mut self, output: &mut Vec<f32>) -> Result<()>;
    fn reset(&mut self);
}

impl TempoProcessor for PhaseVocoder {
    fn sample_rate(&self) -> u32 {
        PhaseVocoder::sample_rate(self)
    }

    fn latency_frames(&self) -> usize {
        PhaseVocoder::latency_frames(self)
    }

    fn set_playback_speed(&mut self, playback_speed: f32) -> Result<()> {
        PhaseVocoder::set_playback_speed(self, playback_speed)
    }

    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()> {
        PhaseVocoder::process(self, input, output)
    }

    fn flush(&mut self, output: &mut Vec<f32>) -> Result<()> {
        PhaseVocoder::flush(self, output)
    }

    fn reset(&mut self) {
        PhaseVocoder::reset(self);
    }
}

impl TempoProcessor for FiveBandTempoProcessor {
    fn sample_rate(&self) -> u32 {
        FiveBandTempoProcessor::sample_rate(self)
    }

    fn latency_frames(&self) -> usize {
        FiveBandTempoProcessor::latency_frames(self)
    }

    fn set_playback_speed(&mut self, playback_speed: f32) -> Result<()> {
        FiveBandTempoProcessor::set_playback_speed(self, playback_speed)
    }

    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()> {
        FiveBandTempoProcessor::process(self, input, output)
    }

    fn flush(&mut self, output: &mut Vec<f32>) -> Result<()> {
        FiveBandTempoProcessor::flush(self, output)
    }

    fn reset(&mut self) {
        FiveBandTempoProcessor::reset(self);
    }
}

pub fn create_tempo_processor(config: TempoProcessorConfig) -> Result<Box<dyn TempoProcessor>> {
    if !config.playback_speed.is_finite()
        || !(MIN_PLAYBACK_SPEED..=MAX_PLAYBACK_SPEED).contains(&config.playback_speed)
    {
        bail!("playback speed must be between {MIN_PLAYBACK_SPEED:.2} and {MAX_PLAYBACK_SPEED:.2}");
    }

    create_tempo_processor_with_layout(config, load_processor_layout()?)
}

fn create_tempo_processor_with_layout(
    config: TempoProcessorConfig,
    layout: ProcessorLayout,
) -> Result<Box<dyn TempoProcessor>> {
    match layout {
        ProcessorLayout::SingleBand(resolution) => {
            Ok(Box::new(PhaseVocoder::new(PhaseVocoderConfig {
                sample_rate: config.sample_rate,
                fft_size: resolution.fft_size,
                analysis_hop: resolution.analysis_hop,
                channel_count: config.channel_count,
                playback_speed: config.playback_speed,
            })?))
        }
        ProcessorLayout::FiveBand {
            analysis,
            crossover_hz,
            filter_tap_count,
        } => {
            let band_analysis = analysis.map(|resolution| BandAnalysisConfig {
                fft_size: resolution.fft_size,
                analysis_hop: resolution.analysis_hop,
            });
            Ok(Box::new(FiveBandTempoProcessor::new(
                FiveBandTempoProcessorConfig {
                    sample_rate: config.sample_rate,
                    channel_count: config.channel_count,
                    playback_speed: config.playback_speed,
                    crossover_hz: crossover_hz.map(|frequency| frequency as f32),
                    filter_tap_count,
                    band_analysis,
                },
            )?))
        }
    }
}

fn load_processor_layout() -> Result<ProcessorLayout> {
    let Some(path) = dsp_config_path() else {
        return Ok(default_processor_layout());
    };
    let input = match fs::read_to_string(&path) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_processor_layout());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    parse_processor_layout(&input)
        .with_context(|| format!("invalid DSP configuration {}", path.display()))
}

fn default_processor_layout() -> ProcessorLayout {
    ProcessorLayout::SingleBand(DEFAULT_ANALYSIS_RESOLUTIONS[0])
}

fn dsp_config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|directory| !directory.is_empty())
                .map(|directory| PathBuf::from(directory).join(".config"))
        })
        .map(|directory| {
            directory
                .join(DSP_CONFIG_DIRECTORY)
                .join(DSP_CONFIG_FILENAME)
        })
}

fn parse_analysis_resolutions(input: &str) -> Result<Vec<AnalysisResolution>> {
    let version = unsigned_integer(input, "version")?;
    if version != DSP_CONFIG_VERSION {
        bail!("unsupported DSP configuration version {version}");
    }

    object_array(input, "phase_vocoders")?
        .iter()
        .enumerate()
        .map(|(index, processor)| {
            let fft_size = positive_usize(processor, "fft_size", index)?;
            let analysis_hop = positive_usize(processor, "analysis_hop", index)?;
            Ok(AnalysisResolution {
                fft_size,
                analysis_hop,
            })
        })
        .collect()
}

fn parse_processor_layout(input: &str) -> Result<ProcessorLayout> {
    let resolutions = parse_analysis_resolutions(input)?;
    let band_count = optional_unsigned_integer(input, "band_count")?.unwrap_or(1);
    match band_count {
        1 => {
            let [resolution] = resolutions.as_slice() else {
                bail!(
                    "the single-band engine requires one phase_vocoders entry, found {}",
                    resolutions.len()
                );
            };
            Ok(ProcessorLayout::SingleBand(*resolution))
        }
        5 => {
            if resolutions.len() != BAND_COUNT {
                bail!(
                    "the five-band engine requires five phase_vocoders entries, found {}",
                    resolutions.len()
                );
            }
            let analysis = std::array::from_fn(|index| resolutions[index]);
            let crossover_values = unsigned_integer_array(input, "crossover_hz")?;
            if crossover_values.len() != BAND_COUNT - 1 {
                bail!(
                    "the five-band engine requires four crossover_hz entries, found {}",
                    crossover_values.len()
                );
            }
            let crossover_hz = std::array::from_fn(|index| crossover_values[index]);
            let filter_tap_count = usize::try_from(unsigned_integer(input, "filter_tap_count")?)
                .context("filter_tap_count is too large")?;
            Ok(ProcessorLayout::FiveBand {
                analysis,
                crossover_hz,
                filter_tap_count,
            })
        }
        band_count => bail!("unsupported DSP band_count {band_count}; expected 1 or 5"),
    }
}

fn positive_usize(json: &str, key: &str, index: usize) -> Result<usize> {
    let value = unsigned_integer(json, key)
        .with_context(|| format!("invalid phase_vocoders entry {index}"))?;
    let value = usize::try_from(value)
        .with_context(|| format!("phase_vocoders entry {index} has an oversized {key}"))?;
    if value == 0 {
        bail!("phase_vocoders entry {index} has a zero {key}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        AnalysisResolution, ProcessorLayout, TempoProcessorConfig, create_tempo_processor,
        create_tempo_processor_with_layout, parse_analysis_resolutions, parse_processor_layout,
    };

    #[test]
    fn configuration_parses_multiple_analysis_resolutions() {
        let resolutions = parse_analysis_resolutions(
            r#"
                {
                  "version": 1,
                  "phase_vocoders": [
                    { "fft_size": 4096, "analysis_hop": 1024 },
                    { "fft_size": 1024, "analysis_hop": 256 }
                  ]
                }
            "#,
        )
        .unwrap();

        assert_eq!(
            resolutions,
            [
                AnalysisResolution {
                    fft_size: 4096,
                    analysis_hop: 1024,
                },
                AnalysisResolution {
                    fft_size: 1024,
                    analysis_hop: 256,
                },
            ]
        );
    }

    #[test]
    fn configuration_selects_an_explicit_five_band_layout() {
        let layout = parse_processor_layout(
            r#"
                {
                  "version": 1,
                  "band_count": 5,
                  "filter_tap_count": 257,
                  "crossover_hz": [150, 600, 2400, 7000],
                  "phase_vocoders": [
                    { "fft_size": 2048, "analysis_hop": 256 },
                    { "fft_size": 2048, "analysis_hop": 256 },
                    { "fft_size": 2048, "analysis_hop": 256 },
                    { "fft_size": 2048, "analysis_hop": 256 },
                    { "fft_size": 2048, "analysis_hop": 256 }
                  ]
                }
            "#,
        )
        .unwrap();

        assert_eq!(
            layout,
            ProcessorLayout::FiveBand {
                analysis: [AnalysisResolution {
                    fft_size: 2048,
                    analysis_hop: 256,
                }; 5],
                crossover_hz: [150, 600, 2400, 7000],
                filter_tap_count: 257,
            }
        );
    }

    #[test]
    fn factory_builds_and_runs_the_selected_five_band_layout() {
        let analysis = AnalysisResolution {
            fft_size: 256,
            analysis_hop: 64,
        };
        let layout = ProcessorLayout::FiveBand {
            analysis: [analysis; 5],
            crossover_hz: [300, 800, 1600, 2800],
            filter_tap_count: 33,
        };
        let mut processor = create_tempo_processor_with_layout(
            TempoProcessorConfig {
                sample_rate: 8_000,
                channel_count: 1,
                playback_speed: 0.75,
            },
            layout,
        )
        .unwrap();
        let input = (0..4_000)
            .map(|frame| (TAU * 440.0 * frame as f32 / 8_000.0).sin() * 0.25)
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        processor.process(&input, &mut output).unwrap();
        processor.flush(&mut output).unwrap();

        assert!(output.len() > input.len());
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 0.01));
    }

    #[test]
    fn factory_hides_the_current_single_band_resolution() {
        let processor = create_tempo_processor(TempoProcessorConfig {
            sample_rate: 48_000,
            channel_count: 2,
            playback_speed: 0.75,
        })
        .unwrap();

        assert_eq!(processor.sample_rate(), 48_000);
        assert!(processor.latency_frames() > 0);
    }

    #[test]
    fn in_code_default_uses_the_documented_resolution() {
        assert_eq!(
            super::DEFAULT_ANALYSIS_RESOLUTIONS,
            [AnalysisResolution {
                fft_size: 2048,
                analysis_hop: 512,
            }]
        );
    }
}
