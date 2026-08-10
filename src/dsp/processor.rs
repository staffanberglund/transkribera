use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::json::{object_array, unsigned_integer};

use super::phase_vocoder::{PhaseVocoder, PhaseVocoderConfig};

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

pub fn create_tempo_processor(config: TempoProcessorConfig) -> Result<Box<dyn TempoProcessor>> {
    if !config.playback_speed.is_finite()
        || !(MIN_PLAYBACK_SPEED..=MAX_PLAYBACK_SPEED).contains(&config.playback_speed)
    {
        bail!("playback speed must be between {MIN_PLAYBACK_SPEED:.2} and {MAX_PLAYBACK_SPEED:.2}");
    }

    let resolutions = load_analysis_resolutions()?;
    let [resolution] = resolutions.as_slice() else {
        bail!(
            "this version requires exactly one configured phase vocoder, found {}",
            resolutions.len()
        );
    };

    Ok(Box::new(PhaseVocoder::new(PhaseVocoderConfig {
        sample_rate: config.sample_rate,
        fft_size: resolution.fft_size,
        analysis_hop: resolution.analysis_hop,
        channel_count: config.channel_count,
        playback_speed: config.playback_speed,
    })?))
}

fn load_analysis_resolutions() -> Result<Vec<AnalysisResolution>> {
    let Some(path) = dsp_config_path() else {
        return Ok(DEFAULT_ANALYSIS_RESOLUTIONS.to_vec());
    };
    let input = match fs::read_to_string(&path) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DEFAULT_ANALYSIS_RESOLUTIONS.to_vec());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    parse_analysis_resolutions(&input)
        .with_context(|| format!("invalid DSP configuration {}", path.display()))
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
    use super::{
        AnalysisResolution, TempoProcessorConfig, create_tempo_processor,
        parse_analysis_resolutions,
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
