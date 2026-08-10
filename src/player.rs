use std::{cell::Cell, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use gstreamer as gst;
use gtk::gio;
use gtk::prelude::*;

use crate::dsp::{
    gst_phase_vocoder::GstPhaseVocoder,
    processor::{MAX_PLAYBACK_SPEED, MIN_PLAYBACK_SPEED},
};

#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    Error(String),
    StateChanged(gst::State),
    DurationChanged,
}

/// A deliberately small wrapper around GStreamer's `playbin` element.
///
/// `playbin` still owns decoding, seeking, and bus/state handling. Its audio
/// sink is an in-process bin containing the Rust phase vocoder.
pub struct Player {
    playbin: gst::Element,
    phase_vocoder: GstPhaseVocoder,
    bus: gst::Bus,
    loaded: Cell<bool>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let playbin = gst::ElementFactory::make("playbin")
            .build()
            .context("the GStreamer playbin element is unavailable")?;
        let (audio_sink, phase_vocoder) = create_audio_sink()?;
        playbin.set_property("audio-sink", &audio_sink);
        let bus = playbin.bus().context("playbin did not provide a bus")?;

        Ok(Self {
            playbin,
            phase_vocoder,
            bus,
            loaded: Cell::new(false),
        })
    }

    pub fn open(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            bail!("the selected file is not accessible");
        }

        self.set_state(gst::State::Null, "reset the previous file")?;
        self.loaded.set(false);

        let file = gio::File::for_path(path);
        let uri = file.uri();
        self.playbin.set_property("uri", uri.as_str());
        self.set_state(gst::State::Paused, "open the audio file")?;
        self.loaded.set(true);
        Ok(())
    }

    pub fn play(&self) -> Result<()> {
        self.ensure_loaded()?;
        self.set_state(gst::State::Playing, "start playback")
    }

    pub fn pause(&self) -> Result<()> {
        self.ensure_loaded()?;
        self.set_state(gst::State::Paused, "pause playback")
    }

    pub fn stop(&self) -> Result<()> {
        self.ensure_loaded()?;
        self.set_state(gst::State::Paused, "stop playback")?;
        self.seek(gst::ClockTime::ZERO)
    }

    pub fn set_volume(&self, volume: f64) {
        self.playbin.set_property("volume", volume.clamp(0.0, 1.0));
    }

    pub fn set_playback_speed(&self, playback_speed: f32) -> Result<()> {
        if !playback_speed.is_finite()
            || !(MIN_PLAYBACK_SPEED..=MAX_PLAYBACK_SPEED).contains(&playback_speed)
        {
            bail!(
                "playback speed must be between {MIN_PLAYBACK_SPEED:.2} and {MAX_PLAYBACK_SPEED:.2}"
            );
        }
        self.phase_vocoder.set_playback_speed(playback_speed)
    }

    pub fn set_tempo_bypass(&self, bypass: bool) -> Result<()> {
        if self.phase_vocoder.bypass() == bypass {
            return Ok(());
        }

        self.phase_vocoder.set_bypass(bypass);
        if let Some(position) = self.position()
            && let Err(error) = self.seek(position)
        {
            self.phase_vocoder.set_bypass(!bypass);
            return Err(error).context("could not restart playback after changing bypass mode");
        }
        Ok(())
    }

    pub fn seek(&self, position: gst::ClockTime) -> Result<()> {
        self.ensure_loaded()?;
        self.phase_vocoder.set_source_position(position);
        self.playbin
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)
            .map_err(|_| anyhow!("GStreamer rejected the seek request"))
    }

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.loaded
            .get()
            .then(|| self.phase_vocoder.source_position())
            .flatten()
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.loaded
            .get()
            .then(|| self.playbin.query_duration::<gst::ClockTime>())
            .flatten()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.get()
    }

    pub fn poll_events(&self) -> Vec<PlayerEvent> {
        let mut events = Vec::new();

        while let Some(message) = self.bus.pop() {
            use gst::MessageView;

            match message.view() {
                MessageView::Eos(..) => events.push(PlayerEvent::EndOfStream),
                MessageView::Error(error) => {
                    let source = error
                        .src()
                        .map(|source| source.path_string())
                        .unwrap_or_else(|| "unknown GStreamer element".into());
                    events.push(PlayerEvent::Error(format!(
                        "{source}: {}{}",
                        error.error(),
                        error
                            .debug()
                            .map(|debug| format!("\n\nDiagnostic details: {debug}"))
                            .unwrap_or_default()
                    )));
                }
                MessageView::StateChanged(change)
                    if message.src().is_some_and(|source| {
                        source == self.playbin.upcast_ref::<gst::Object>()
                    }) =>
                {
                    events.push(PlayerEvent::StateChanged(change.current()));
                }
                MessageView::DurationChanged(..) => events.push(PlayerEvent::DurationChanged),
                _ => {}
            }
        }

        events
    }

    fn ensure_loaded(&self) -> Result<()> {
        if self.loaded.get() {
            Ok(())
        } else {
            bail!("open an audio file first")
        }
    }

    fn set_state(&self, state: gst::State, action: &str) -> Result<()> {
        self.playbin
            .set_state(state)
            .map(|_| ())
            .map_err(|error| anyhow!("could not {action}: {error:?}"))
    }
}

fn create_audio_sink() -> Result<(gst::Bin, GstPhaseVocoder)> {
    let bin = gst::Bin::with_name("phase-vocoder-audio-sink");
    let convert_in = gst::ElementFactory::make("audioconvert")
        .name("phase-vocoder-input-convert")
        .build()
        .context("the GStreamer audioconvert element is unavailable")?;
    let resample_in = gst::ElementFactory::make("audioresample")
        .name("phase-vocoder-input-resample")
        .build()
        .context("the GStreamer audioresample element is unavailable")?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .name("phase-vocoder-format")
        .build()
        .context("the GStreamer capsfilter element is unavailable")?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("channels", gst::IntRange::<i32>::new(1, 2))
        .build();
    caps_filter.set_property("caps", &caps);

    let phase_element = gst::ElementFactory::make("rustphasevocoder")
        .name("phase-vocoder")
        .build()
        .context("the in-process phase-vocoder element is unavailable")?;
    let phase_vocoder = phase_element
        .clone()
        .downcast::<GstPhaseVocoder>()
        .map_err(|_| anyhow!("registered phase-vocoder element has the wrong type"))?;
    let convert_out = gst::ElementFactory::make("audioconvert")
        .name("phase-vocoder-output-convert")
        .build()
        .context("the output audioconvert element is unavailable")?;
    let resample_out = gst::ElementFactory::make("audioresample")
        .name("phase-vocoder-output-resample")
        .build()
        .context("the output audioresample element is unavailable")?;
    let sink = gst::ElementFactory::make("autoaudiosink")
        .name("phase-vocoder-device-sink")
        .build()
        .context("the GStreamer automatic audio sink is unavailable")?;

    let elements = [
        convert_in.clone(),
        resample_in,
        caps_filter,
        phase_element,
        convert_out,
        resample_out,
        sink,
    ];
    bin.add_many(elements.iter())
        .context("could not add elements to the phase-vocoder audio sink")?;
    gst::Element::link_many(elements.iter())
        .context("could not link the phase-vocoder audio sink")?;

    let sink_pad = convert_in
        .static_pad("sink")
        .context("input audioconvert has no sink pad")?;
    let ghost_pad = gst::GhostPad::with_target(&sink_pad)
        .context("could not create the audio-sink ghost pad")?;
    ghost_pad
        .set_active(true)
        .context("could not activate the audio-sink ghost pad")?;
    bin.add_pad(&ghost_pad)
        .context("could not add the ghost pad to the audio-sink bin")?;

    Ok((bin, phase_vocoder))
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Err(error) = self.playbin.set_state(gst::State::Null) {
            log::warn!("could not stop GStreamer while closing: {error:?}");
        }
    }
}
