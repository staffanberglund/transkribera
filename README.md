# Transkribera

Transkribera is a GTK 4 desktop audio player intended as a small,
maintainable foundation for transcription work. It uses GStreamer for both
playback and waveform decoding and deliberately omits editing and advanced
playback features.

## Current functionality

- Opens local MP3, FLAC, Opus, and Ogg files with GTK's file chooser.
- Keeps a persistent menu of the ten most recently opened audio files.
- Plays, pauses, stops, jumps to the beginning or end, and provides volume
  control.
- Changes playback tempo from 0.25× to 1.50× with an in-project phase vocoder
  while preserving pitch, with an explicit checkbox to bypass tempo processing
  and play decoded PCM directly.
- Seeks with a slider, a click on the waveform, `J`/`L` for ±10 seconds,
  Shift+Left/Right for ±5 seconds, or Left/Right for ±1 second.
- Zooms the waveform continuously from 1× to 100×. Slider changes center the
  playhead during playback and otherwise center the playback anchor;
  `Ctrl`+scroll and touchpad pinch preserve the time beneath the pointer.
- Pans a zoomed waveform using two-finger scrolling or the timeline scrollbar.
- Remembers a playback anchor: `K` toggles at the current position, `Space`
  pauses or restarts from the anchor, and `P` always restarts from the anchor.
- Adds named, clickable, and deletable timeline markers at the playhead, jumps
  between them with `Alt`+Left/Right or Page Up/Down, and saves them
  automatically as JSON for each audio file. Right-clicking a marker opens a
  Rename menu; its pen button is a direct rename shortcut.
- Creates persistent A–B loops by dragging across the waveform. Saved loops
  appear beside markers; selecting one enables repetition, and its A and B
  handles can be dragged independently for precise adjustment.
- Collapses the marker list when it is not needed or resizes it by dragging the
  divider between the waveform controls and marker pane.
- Offers persistent settings for marker-name prompting and editable keyboard
  shortcuts. Additional key combinations can be assigned to any supported
  playback, seeking, or marker command.
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
flatpak-builder --user --install --force-clean build-dir flatpak/io.github.staffanberglund.transkribera.json
flatpak run --user io.github.staffanberglund.transkribera
```

For faster development cycles, use the incremental debug build script:

```bash
./dev-build.sh
```

The first invocation prepares `build-dir` if necessary and performs a full
debug build. Later invocations reuse Cargo artifacts in
`target/flatpak-dev`, install only the changed build into the writable Flatpak
tree, strip the installed copy while retaining local debug information, export
it, update the user installation, and launch the app. Use
`./dev-build.sh --no-run` to build and install without launching it.

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
  volume, seeking, source-time position/duration queries, and non-blocking bus
  events. It replaces only `playbin`'s audio sink with the phase-vocoder bin.
- `src/dsp/phase_vocoder.rs` contains the GStreamer-independent STFT phase
  vocoder. Each instance receives an explicit sample rate, FFT size, analysis
  hop, channel count, and initial playback speed.
- `src/dsp/processor.rs` is the pure-DSP composition boundary. GStreamer owns a
  boxed `TempoProcessor` and cannot depend on how many phase vocoders it
  contains or which analysis resolutions they use. The factory creates the
  default single-band processor or the explicitly selected five-band processor
  behind this same interface.
- `src/dsp/processor.rs` also supplies the default phase-vocoder analysis
  resolution and loads an optional per-user `dsp.json` override.
- `src/dsp/filter_bank.rs` contains the isolated streaming five-band
  complementary FIR filter bank.
- `src/dsp/multiband.rs` composes that filter bank with five full-rate phase
  vocoders and enforces sample-count alignment before summing their output.
- `src/dsp/onset_detector.rs` timestamps attacks once on the original full-band
  signal so different analysis resolutions follow one transient timeline.
- `src/dsp/window.rs` generates the periodic Hann window.
- `src/dsp/gst_phase_vocoder.rs` wraps the DSP in a statically registered,
  in-process GStreamer element accepting interleaved mono or stereo F32LE PCM.
- `src/markers.rs` loads and atomically saves a versioned JSON marker list in
  the application's XDG data directory, keyed by the audio file's canonical
  path.
- `src/loops.rs` loads and atomically saves named A–B regions per audio file.
- `src/preferences.rs` persists the small application settings file alongside
  the marker directory.
- `src/recent.rs` persists and updates the most-recent-first audio file list.
- `src/shortcuts.rs` defines the editable command catalog, default key bindings,
  and GTK accelerator matching used by the settings UI.
- `src/waveform.rs` creates a separate `uridecodebin` pipeline ending in an
  `appsink`. A worker decodes mono 8 kHz float PCM, retains fixed-window peaks,
  retains peaks at roughly 8 ms intervals, reduces very long files to at most
  524,288 values, and sends the result back to the UI.

Opening another file stops the previous `playbin`, clears the UI, and cancels the
previous waveform job. Closing the window drops the job's cancellation token;
the worker shuts down at its next sample/poll boundary.

## Tempo and phase-vocoder design

The speed control uses this convention:

```text
playback_speed = output tempo / source tempo
1.00× = original tempo
0.75× = slower
1.25× = faster
```

The Bypass checkbox temporarily disables the speed control and forwards decoded
PCM buffers through the custom GStreamer element without invoking the DSP. The
selected speed is retained for when bypass is disabled. Changing modes performs
a flush seek at the current source position so overlap-buffered phase-vocoder
audio is not mixed with direct audio.

The in-code default uses a 2,048-sample FFT and a 512-sample analysis hop. To
override it without rebuilding, create
`$XDG_CONFIG_HOME/transkribera/dsp.json` (normally
`~/.config/transkribera/dsp.json`; inside Flatpak it is stored below
`~/.var/app/io.github.staffanberglund.transkribera/config/`) with this structure:

```json
{
  "version": 1,
  "phase_vocoders": [
    {
      "fft_size": 2048,
      "analysis_hop": 512
    }
  ]
}
```

The override is read when an audio stream is opened. Omitting `band_count`
selects the single-band engine and requires exactly one `phase_vocoders` entry.
The phase-vocoder unit uses a periodic Hann window and its synthesis hop is:

```text
synthesis_hop = analysis_hop / playback_speed
```

At unity speed it copies the analysis phases directly, making the steady-state
STFT reconstruction transparent instead of allowing small phase-integration
errors to accumulate. At other speeds it uses identity phase locking: spectral
peaks follow the normal phase-vocoder trajectory, while nearby bins retain
their analysis-frame phase relationship to the peak. This reduces the phasy
level modulation produced when every FFT bin evolves independently. Playback
speeds within `0.0001` of unity are treated as exact unity.

At non-unity speeds, a normalized positive-spectral-flux detector identifies
new attacks independently in either input channel. A detected onset resets the
synthesis phases for both stereo channels on the same analysis frame, reducing
the pre-echo and phase smearing of transients without moving the stereo image.
The detector has a 30 ms source-time retrigger interval; steady tonal frames
continue through identity phase locking without resets.

The single-band engine performs that detection internally. The five-band
engine instead runs one short-window detector on the original full-band signal,
adds the filter-bank group delay to each source-frame timestamp, and supplies
the resulting ordered event timeline to all five phase vocoders. This prevents
the different FFT resolutions from independently placing the same attack on
different detected onsets.

The five-band engine also gives every resolution one shared source-to-output
tempo map. Each synthesis window is placed according to its analysis-window
center on that absolute map, and integer hops are derived from rounded absolute
positions rather than accumulated independently. This keeps broadband events
aligned across FFT sizes at fixed speeds and across live speed changes.

Fractional hops are carried between frames so they do not accumulate a duration
error. Exponential smoothing with a 50 ms source-time constant softens changes
made after processing has started. Its per-frame coefficient is derived from
the sample rate and analysis hop, so changing either does not change the
transition duration.

For each channel independently, the processor buffers overlapping frames,
windows them, performs a forward FFT, extracts magnitude and phase, subtracts
the expected bin-phase advance, wraps the deviation into `[-π, π)`, estimates
instantaneous frequency, accumulates phase using the synthesis hop, reconstructs
the conjugate-symmetric spectrum, performs the inverse FFT, applies the Hann
window again, and overlap-adds with per-sample window-power normalization.

The initial input fill is 2,048 samples (about 42.7 ms at 48 kHz). The element
reports the steady-state overlap latency of 1,536 samples (about 32 ms at
48 kHz) through GStreamer's latency query.

### Five-band filter-bank foundation

The next-generation stretch engine begins with a pure-DSP five-band filter
bank. It accepts a sample rate, channel count, four strictly increasing
crossover frequencies, and an odd FIR tap count. All four cumulative low-pass
filters use equal-length, symmetric Blackman-windowed sinc kernels, giving them
the same linear-phase group delay:

```text
delay = (tap_count - 1) / 2 frames

band 1 = L1
band 2 = L2 - L1
band 3 = L3 - L2
band 4 = L4 - L3
band 5 = delayed input - L4
```

Summing the bands therefore telescopes exactly to the delayed input regardless
of the individual crossover shapes. The streaming implementation preserves
interleaved channels, accepts arbitrary complete-frame chunk sizes, emits the
full FIR tails on flush, and clears all history on reset.

Automated tests cover impulse reconstruction, random chunked stereo
reconstruction, output-length stability, reset, invalid configurations, and
tone isolation. The direct-convolution implementation and full-rate band
outputs deliberately favor a simple verifiable foundation. FIR optimization
and downsampling are later stages.

### Experimental five-band tempo processor

The opt-in multiband processor runs each full-rate filter-bank output through
its own phase vocoder. Persistent queues absorb the different block-production
schedules: during streaming it sums samples available from every band, and at
end-of-stream it drains the longest DSP tail while padding shorter tails with
silence. Its reported latency is the FIR group delay plus the largest
phase-vocoder latency. Speed changes, flush, and reset are forwarded to all
five processors.

The bands may use different FFT and hop settings while they still run at the
common input sample rate. For manual testing at a 48 kHz input rate, use this
`dsp.json` override:

```json
{
  "version": 1,
  "band_count": 5,
  "filter_tap_count": 257,
  "crossover_hz": [150, 600, 2400, 7000],
  "phase_vocoders": [
    { "fft_size": 8192, "analysis_hop": 1024 },
    { "fft_size": 4096, "analysis_hop": 512 },
    { "fft_size": 2048, "analysis_hop": 256 },
    { "fft_size": 1024, "analysis_hop": 128 },
    { "fft_size": 512,  "analysis_hop": 64 }
  ]
}
```

Remove `band_count`, the crossover/filter fields, and four of the processor
entries to return to the single-band engine. This is not yet the final
multirate engine: all bands still run at the source sample rate, and per-band
downsampling and reconstruction filters remain a later stage.

### GStreamer integration and time

The application registers `rustphasevocoder` statically after `gst::init()`; it
does not install or discover an external plugin. `playbin` continues to select
demuxers and decoders. Its custom audio sink is:

```text
playbin decoded audio
  → audioconvert
  → audioresample
  → audio/x-raw, format=F32LE, layout=interleaved, channels=1..2
  → rustphasevocoder
  → audioconvert
  → audioresample
  → autoaudiosink
```

Input buffer timestamps remain source-media time. Output timestamps begin at
the current source segment start and then advance by the duration of the
produced PCM. Slower playback therefore produces more output clock time and
faster playback produces less. The player deliberately reports the latest
input/source timestamp to the UI, so the seek slider, waveform, markers, and
time label always refer to the original media timeline. Duration queries and
seek requests also remain in source time.

Flush, new-segment, discontinuity, stop, state reset, and opening another file
clear pending input, phase history, overlap-add data, and pending output. EOS
pads and processes the final partial frame, emits the remaining overlap-add
tail, and then forwards EOS.

Marker files are stored under
`$XDG_DATA_HOME/transkribera/markers/` when running natively. The Flatpak
location is normally
`~/.var/app/io.github.staffanberglund.transkribera/data/transkribera/markers/`.
Each JSON file records the source path plus marker names and positions in
nanoseconds. Saved A–B regions use the parallel `loops/` directory and record
their names and endpoints in nanoseconds. Application settings are stored in
`settings.json` one directory above these directories, with recent paths in
`recent.json` beside it.

## Known limitations

- Waveforms appear after full-file analysis; generation is not progressive.
- Waveform peaks are optimized for navigation, not sample-accurate editing.
- Duration and seeking depend on what the selected container/decoder reports.
- Markers do not yet support notes or drag-to-reposition.
- There are no playlists, independent pitch controls, or destructive editing
  tools.
- The basic phase vocoder can smear sharp transients, sound phasy on complex
  material, and make the stereo image less stable because channels are
  processed independently. Artifacts are strongest near 0.25× and 1.50×.
- Only mono and stereo playback are negotiated through the phase vocoder.

## Manual tempo test procedure

Test spoken voice, sustained music, percussion-heavy music, and stereo music
in MP3, FLAC, and Opus/Ogg form. For each file, try 0.50×, 0.75×, 1.00×, 1.25×,
and 1.50× and check:

1. Pitch remains approximately unchanged while tempo changes.
2. Playback remains stable while repeatedly moving the speed slider.
3. The position display and waveform continue to show source-media time.
4. Waveform clicks, the seek slider, and keyboard seeks work at every speed.
5. Pause/resume, stop, end of stream, and opening another file do not replay
   stale audio.
6. Stereo channels remain distinct and the GTK interface stays responsive.
7. The same checks pass in the installed Flatpak.

Automated DSP tests use generated sine waves to validate phase wrapping,
unity/slow/fast duration, dominant pitch, finite and reasonable gain, stereo
interleaving, and reset behavior. A headless GStreamer test validates F32LE
stereo negotiation, processing, draining, and EOS.

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
flatpak run --command=gst-inspect-1.0 io.github.staffanberglund.transkribera opusdec
```

If the chooser opens but the app cannot access a selected file, ensure a desktop
portal and an appropriate backend are running, then inspect permissions with
`flatpak info --show-permissions io.github.staffanberglund.transkribera`.

## License

This project is free software licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
