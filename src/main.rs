mod application;
mod dsp;
mod json;
mod loops;
mod markers;
mod player;
mod preferences;
mod recent;
mod shortcuts;
mod waveform;

use anyhow::Context;

fn main() {
    env_logger::init();

    if let Err(error) = run() {
        eprintln!("Failed to start Transkribera: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    gstreamer::init().context("could not initialize GStreamer")?;
    dsp::gst_phase_vocoder::register()?;
    application::run();
    Ok(())
}
