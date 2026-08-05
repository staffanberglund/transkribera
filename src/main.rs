mod application;
mod player;
mod waveform;

use anyhow::Context;

fn main() {
    env_logger::init();

    if let Err(error) = run() {
        eprintln!("Failed to start Transcription MVP: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    gstreamer::init().context("could not initialize GStreamer")?;
    application::run();
    Ok(())
}
