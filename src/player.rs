use std::{cell::Cell, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use gstreamer as gst;
use gtk::gio;
use gtk::prelude::*;

#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    Error(String),
    StateChanged(gst::State),
    DurationChanged,
}

/// A deliberately small wrapper around GStreamer's `playbin` element.
pub struct Player {
    playbin: gst::Element,
    bus: gst::Bus,
    loaded: Cell<bool>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let playbin = gst::ElementFactory::make("playbin")
            .build()
            .context("the GStreamer playbin element is unavailable")?;
        let bus = playbin.bus().context("playbin did not provide a bus")?;

        Ok(Self {
            playbin,
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

    pub fn seek(&self, position: gst::ClockTime) -> Result<()> {
        self.ensure_loaded()?;
        self.playbin
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)
            .map_err(|_| anyhow!("GStreamer rejected the seek request"))
    }

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.loaded
            .get()
            .then(|| self.playbin.query_position::<gst::ClockTime>())
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

impl Drop for Player {
    fn drop(&mut self) {
        if let Err(error) = self.playbin.set_state(gst::State::Null) {
            log::warn!("could not stop GStreamer while closing: {error:?}");
        }
    }
}
