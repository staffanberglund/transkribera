mod file_ui;
mod loop_ui;
mod marker_ui;
mod navigation;
mod playback_ui;
mod settings_ui;
mod waveform_interaction;
mod window;

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};

use gstreamer as gst;
use gtk::{gio, prelude::*};

use crate::{
    loops::{LoopRegion, LoopStore},
    markers::{Marker, MarkerStore},
    player::Player,
    preferences::PreferencesStore,
    recent::RecentStore,
    shortcuts::KeyBinding,
    waveform::WaveformJob,
};

const APP_ID: &str = "io.github.staffanberglund.transkribera";
const MIN_WAVEFORM_ZOOM: f64 = 1.0;

pub fn run() {
    let application = gtk::Application::builder().application_id(APP_ID).build();
    application.connect_activate(window::build_ui);
    application.run();
}

struct UiState {
    window: gtk::ApplicationWindow,
    player: Player,
    beginning_button: gtk::Button,
    play_button: gtk::Button,
    stop_button: gtk::Button,
    end_button: gtk::Button,
    add_marker_button: gtk::Button,
    marker_list: gtk::ListBox,
    loop_list: gtk::ListBox,
    recent_button: gtk::MenuButton,
    recent_menu: gio::Menu,
    settings_window: RefCell<Option<gtk::Window>>,
    seek: gtk::Scale,
    time_label: gtk::Label,
    waveform: gtk::DrawingArea,
    waveform_adjustment: gtk::Adjustment,
    waveform_zoom: gtk::Scale,
    spinner: gtk::Spinner,
    duration: Cell<Option<gst::ClockTime>>,
    peaks: RefCell<Vec<f32>>,
    markers: RefCell<Vec<Marker>>,
    marker_store: RefCell<Option<MarkerStore>>,
    loops: RefCell<Vec<LoopRegion>>,
    loop_store: RefCell<Option<LoopStore>>,
    active_loop: Cell<Option<usize>>,
    loop_drag: RefCell<Option<LoopDrag>>,
    loop_preview: Cell<Option<(u64, u64)>>,
    preferences_store: PreferencesStore,
    prompt_for_marker_name: Cell<bool>,
    key_bindings: RefCell<Vec<KeyBinding>>,
    recent_store: RecentStore,
    recent_files: RefCell<Vec<PathBuf>>,
    progress: Cell<f64>,
    playback_anchor: Cell<gst::ClockTime>,
    anchor_progress: Cell<f64>,
    playing: Cell<bool>,
    follow_playback: Cell<bool>,
    playhead_tick: RefCell<Option<gtk::TickCallbackId>>,
    pending_zoom_focus: Cell<Option<(f64, f64)>>,
    waveform_pointer: Cell<f64>,
    user_seeking: Cell<bool>,
    seek_change_serial: Cell<u64>,
    waveform_job: RefCell<Option<WaveformJob>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopDrag {
    // A drag either creates a region or moves one handle of the active region.
    New { start_ns: u64 },
    Start { index: usize },
    End { index: usize },
}

fn poll_waveform_job(state: &Rc<UiState>) {
    let result = state
        .waveform_job
        .borrow()
        .as_ref()
        .and_then(WaveformJob::try_result);

    let Some(result) = result else {
        return;
    };

    state.waveform_job.borrow_mut().take();
    state.spinner.stop();
    state.spinner.set_visible(false);

    match result {
        Ok(peaks) => {
            state.peaks.replace(peaks);
            state.waveform.queue_draw();
        }
        Err(error) => {
            log::warn!("waveform generation failed: {error:#}");
            show_error(
                &state.window,
                "Waveform unavailable",
                &format!("Playback may still work.\n\n{error:#}"),
            );
        }
    }
}

fn show_error(parent: &impl IsA<gtk::Window>, title: &str, details: &str) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(details)
        .build();
    dialog.show(Some(parent));
}
