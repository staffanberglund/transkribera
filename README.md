# Transcription MVP

Transcription MVP is a minimal GTK 4 desktop audio player intended as a small,
maintainable foundation for transcription work. It uses GStreamer for both
playback and waveform decoding and deliberately omits editing and advanced
playback features.

## Current functionality

- Opens local MP3, FLAC, Opus, and Ogg files with GTK's file chooser.
- Plays, pauses, stops, and seeks with a slider or a click on the waveform.
- Zooms the waveform continuously from 1× to 100×. Slider changes center the
  playhead during playback and otherwise center the playback anchor;
  `Ctrl`+scroll preserves the time beneath the pointer.
- Pans a zoomed waveform using two-finger scrolling or the timeline scrollbar.
- Remembers a playback anchor: `K` toggles at the current position, `Space`
  pauses or restarts from the anchor, and `P` always restarts from the anchor.
- Shows elapsed and total time and highlights waveform playback progress.
- Decodes a normalized, reduced mono waveform on a worker thread.
- Reports playback and waveform errors without terminating the application.
- Builds offline inside Flatpak from the checked-in Cargo lockfile and `vendor/`.

## Native development

Install a stable Rust toolchain and development packages for GTK 4, GStreamer,
GStreamer App, and the standard GStreamer plugin sets. Package names vary by
distribution. The required pkg-config modules are:

```text
gtk4 >= 4.10
gstreamer-1.0
gstreamer-app-1.0
```

Build and run:

```bash
cargo fmt -- --check
cargo check --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline
cargo run --locked --offline
```

Cargo uses the checked-in `vendor/` directory through `.cargo/config.toml`, so
these commands do not contact crates.io.

## Flatpak

The manifest uses GNOME Platform/SDK 49, based on Freedesktop 25.08, plus the
matching Freedesktop stable Rust SDK extension. Install the build tools and
runtimes, if needed:

```bash
flatpak install --user flathub org.gnome.Platform//49 org.gnome.Sdk//49 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
```

Build, install, and run from the repository root:

```bash
flatpak-builder --user --install --force-clean build-dir flatpak/io.github.example.TranscriptionMvp.json
flatpak run --user io.github.example.TranscriptionMvp
```

The app requests display, GPU, and PulseAudio sockets only. It does not request
`--filesystem=host`; GTK's file dialog grants access to the selected file through
the document portal.

## Supported formats and codecs

The GNOME 49 runtime carries the GStreamer plugins used here:

- MP3: `mpg123audiodec` from GStreamer Ugly plugins.
- FLAC: `flacdec` from GStreamer Good plugins.
- Opus: `opusdec` from GStreamer Base plugins.
- Ogg containers: `oggdemux` from GStreamer Good plugins.

The matching `org.freedesktop.Platform.codecs-extra//25.08-extra` extension can
provide additional codecs, but it is not required for these four formats on the
declared runtime. The application never relies on host-installed plugins.

## Architecture

- `src/application.rs` builds the single GTK window, owns UI state, processes
  player events on a 150 ms GLib timer, and renders the visible interval of a
  fixed-size waveform viewport with Cairo.
- `src/player.rs` is a small `playbin` wrapper for file loading, transport,
  seeking, position/duration queries, and non-blocking bus event collection.
- `src/waveform.rs` creates a separate `uridecodebin` pipeline ending in an
  `appsink`. A worker decodes mono 8 kHz float PCM, retains fixed-window peaks,
  retains peaks at roughly 8 ms intervals, reduces very long files to at most
  524,288 values, and sends the result back to the UI.

Opening another file stops the previous `playbin`, clears the UI, and cancels the
previous waveform job. Closing the window drops the job's cancellation token;
the worker shuts down at its next sample/poll boundary.

## Known limitations

- Waveforms appear after full-file analysis; generation is not progressive.
- Waveform peaks are optimized for navigation, not sample-accurate editing.
- Duration and seeking depend on what the selected container/decoder reports.
- There are no playlists, loops, markers, speed/pitch controls, or editing tools.

## Troubleshooting

For native installations, confirm the required elements are visible:

```bash
gst-inspect-1.0 playbin
gst-inspect-1.0 mpg123audiodec
gst-inspect-1.0 flacdec
gst-inspect-1.0 opusdec
gst-inspect-1.0 oggdemux
```

Run with `RUST_LOG=debug` for application diagnostics and `GST_DEBUG=2` for
GStreamer diagnostics. If Flatpak cannot play audio, verify that the matching
runtime is installed with `flatpak info org.gnome.Platform//49`, update it with
`flatpak update`, and inspect the sandbox copy of a plugin with:

```bash
flatpak run --command=gst-inspect-1.0 io.github.example.TranscriptionMvp opusdec
```

If the chooser opens but the app cannot access a selected file, ensure a desktop
portal and an appropriate backend are running, then inspect permissions with
`flatpak info --show-permissions io.github.example.TranscriptionMvp`.

## License

This project is free software licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
