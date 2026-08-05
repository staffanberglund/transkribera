use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

const SAMPLE_RATE: i32 = 8_000;
const SAMPLES_PER_PEAK: usize = 64;
const MAX_PEAKS: usize = 524_288;

pub struct WaveformJob {
    receiver: Receiver<Result<Vec<f32>>>,
    cancelled: Arc<AtomicBool>,
}

impl WaveformJob {
    pub fn start(uri: String) -> Self {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);

        thread::spawn(move || {
            let result = decode_peaks(&uri, &worker_cancelled);
            let _ = sender.send(result);
        });

        Self {
            receiver,
            cancelled,
        }
    }

    pub fn try_result(&self) -> Option<Result<Vec<f32>>> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for WaveformJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn decode_peaks(uri: &str, cancelled: &AtomicBool) -> Result<Vec<f32>> {
    let pipeline = gst::Pipeline::new();
    let decodebin = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()
        .context("the GStreamer uridecodebin element is unavailable")?;
    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .context("the GStreamer audioconvert element is unavailable")?;
    let resample = gst::ElementFactory::make("audioresample")
        .build()
        .context("the GStreamer audioresample element is unavailable")?;
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .build()
        .context("the GStreamer capsfilter element is unavailable")?;
    let appsink = gst::ElementFactory::make("appsink")
        .property("sync", false)
        .property("max-buffers", 16_u32)
        .property("drop", false)
        .build()
        .context("the GStreamer appsink element is unavailable")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("the created appsink has an unexpected type"))?;

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("channels", 1_i32)
        .field("rate", SAMPLE_RATE)
        .build();
    capsfilter.set_property("caps", &caps);

    pipeline
        .add_many([
            &decodebin,
            &convert,
            &resample,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .context("could not assemble the waveform decoding pipeline")?;
    gst::Element::link_many([&convert, &resample, &capsfilter, appsink.upcast_ref()])
        .context("could not link the waveform decoding pipeline")?;

    let convert_sink = convert
        .static_pad("sink")
        .context("audioconvert has no sink pad")?;
    decodebin.connect_pad_added(move |_decodebin, source_pad| {
        if !convert_sink.is_linked()
            && source_pad
                .current_caps()
                .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("audio/")))
                .unwrap_or(false)
            && source_pad.link(&convert_sink).is_err()
        {
            log::warn!("could not connect the decoded audio stream to waveform analysis");
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| anyhow!("could not start waveform decoding: {error:?}"))?;

    let result = collect_peaks(&pipeline, &appsink, cancelled);
    if let Err(error) = pipeline.set_state(gst::State::Null) {
        log::warn!("could not stop the waveform pipeline: {error:?}");
    }
    result
}

fn collect_peaks(
    pipeline: &gst::Pipeline,
    appsink: &gst_app::AppSink,
    cancelled: &AtomicBool,
) -> Result<Vec<f32>> {
    let bus = pipeline
        .bus()
        .context("the waveform pipeline did not provide a bus")?;
    let mut peaks = Vec::new();
    let mut window_peak = 0.0_f32;
    let mut window_samples = 0_usize;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            bail!("waveform analysis was cancelled");
        }

        if let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            let buffer = sample
                .buffer()
                .context("decoded audio sample has no buffer")?;
            let map = buffer
                .map_readable()
                .context("could not read a decoded audio buffer")?;

            for bytes in map.as_slice().chunks_exact(size_of::<f32>()) {
                let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                if value.is_finite() {
                    window_peak = window_peak.max(value.abs());
                }
                window_samples += 1;

                if window_samples == SAMPLES_PER_PEAK {
                    peaks.push(window_peak);
                    window_peak = 0.0;
                    window_samples = 0;
                }
            }
        }

        while let Some(message) = bus.pop() {
            use gst::MessageView;

            match message.view() {
                MessageView::Eos(..) => {
                    if window_samples > 0 {
                        peaks.push(window_peak);
                    }
                    return normalize_and_reduce(peaks);
                }
                MessageView::Error(error) => {
                    let details = error
                        .debug()
                        .map(|debug| format!(" ({debug})"))
                        .unwrap_or_default();
                    bail!(
                        "GStreamer could not decode the waveform: {}{details}",
                        error.error()
                    );
                }
                _ => {}
            }
        }
    }
}

fn normalize_and_reduce(mut peaks: Vec<f32>) -> Result<Vec<f32>> {
    if peaks.is_empty() {
        bail!("the file contained no decodable audio samples");
    }

    if peaks.len() > MAX_PEAKS {
        let bucket_size = peaks.len().div_ceil(MAX_PEAKS);
        peaks = peaks
            .chunks(bucket_size)
            .map(|bucket| bucket.iter().copied().fold(0.0_f32, f32::max))
            .take(MAX_PEAKS)
            .collect();
    }

    let maximum = peaks.iter().copied().fold(0.0_f32, f32::max);
    if maximum > f32::EPSILON {
        for peak in &mut peaks {
            *peak = (*peak / maximum).clamp(0.0, 1.0);
        }
    } else {
        peaks.fill(0.0);
    }

    peaks.shrink_to_fit();
    Ok(peaks)
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::TAU, fs, sync::atomic::AtomicBool};

    use anyhow::Context;

    use super::*;

    #[test]
    fn decodes_a_waveform_end_to_end() -> Result<()> {
        gst::init().context("could not initialize GStreamer for the test")?;

        let path = std::env::temp_dir().join(format!(
            "transcription-mvp-waveform-test-{}.wav",
            std::process::id()
        ));
        fs::write(&path, sine_wave_wav()).context("could not write the test wave file")?;
        let uri = gst::glib::filename_to_uri(&path, None)
            .map_err(|error| anyhow!("could not create the test file URI: {error}"))?;

        let result = decode_peaks(uri.as_str(), &AtomicBool::new(false));
        if let Err(error) = fs::remove_file(&path) {
            log::warn!("could not remove waveform test fixture: {error}");
        }

        let peaks = result?;
        assert!(peaks.len() >= 100);
        assert!(peaks.iter().any(|peak| *peak > 0.9));
        assert!(peaks.iter().all(|peak| (0.0..=1.0).contains(peak)));
        Ok(())
    }

    fn sine_wave_wav() -> Vec<u8> {
        const SAMPLE_COUNT: u32 = SAMPLE_RATE as u32;
        const BYTES_PER_SAMPLE: u32 = 2;
        let data_size = SAMPLE_COUNT * BYTES_PER_SAMPLE;
        let mut wav = Vec::with_capacity(44 + data_size as usize);

        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE as u32 * BYTES_PER_SAMPLE).to_le_bytes());
        wav.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());

        for sample in 0..SAMPLE_COUNT {
            let phase = sample as f32 / SAMPLE_RATE as f32;
            let value = (phase * 440.0 * TAU).sin() * i16::MAX as f32 * 0.75;
            wav.extend_from_slice(&(value as i16).to_le_bytes());
        }

        wav
    }
}
