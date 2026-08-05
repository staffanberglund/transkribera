use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
};

use gstreamer as gst;
use gtk::{cairo, gio, glib, prelude::*};

use crate::{
    player::{Player, PlayerEvent},
    waveform::WaveformJob,
};

const APP_ID: &str = "io.github.example.TranscriptionMvp";
const UPDATE_INTERVAL_MS: u64 = 150;
const MIN_WAVEFORM_ZOOM: f64 = 1.0;
const MAX_WAVEFORM_ZOOM: f64 = 100.0;
const ZOOM_OCTAVE_SCROLL_UNITS: f64 = 4.0;
const PAN_FRACTION_PER_SCROLL_UNIT: f64 = 0.1;

pub fn run() {
    let application = gtk::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
    application.run();
}

struct UiState {
    window: gtk::ApplicationWindow,
    player: Player,
    play_button: gtk::Button,
    stop_button: gtk::Button,
    seek: gtk::Scale,
    time_label: gtk::Label,
    waveform: gtk::DrawingArea,
    waveform_adjustment: gtk::Adjustment,
    waveform_zoom: gtk::Scale,
    spinner: gtk::Spinner,
    peaks: RefCell<Vec<f32>>,
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

fn build_ui(application: &gtk::Application) {
    let player = match Player::new() {
        Ok(player) => player,
        Err(error) => {
            show_startup_error(application, &error.to_string());
            return;
        }
    };

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Transcription MVP")
        .default_width(720)
        .default_height(360)
        .width_request(360)
        .height_request(260)
        .build();

    let header = gtk::HeaderBar::new();
    let open_button = gtk::Button::builder()
        .label("Open")
        .tooltip_text("Open an audio file")
        .build();
    header.pack_start(&open_button);
    window.set_titlebar(Some(&header));

    let waveform = gtk::DrawingArea::builder()
        .content_width(600)
        .content_height(180)
        .hexpand(true)
        .vexpand(true)
        .build();
    waveform.set_tooltip_text(Some(
        "Click to seek; two-finger scroll to pan; Ctrl+scroll to zoom",
    ));

    let spinner = gtk::Spinner::new();
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_visible(false);

    let waveform_overlay = gtk::Overlay::new();
    waveform_overlay.set_child(Some(&waveform));
    waveform_overlay.add_overlay(&spinner);

    let waveform_frame = gtk::Frame::new(None);
    waveform_frame.set_hexpand(true);
    waveform_frame.set_vexpand(true);
    waveform_frame.set_child(Some(&waveform_overlay));

    let waveform_adjustment = gtk::Adjustment::new(0.0, 0.0, 1.0, 0.01, 0.9, 1.0);
    let waveform_scrollbar =
        gtk::Scrollbar::new(gtk::Orientation::Horizontal, Some(&waveform_adjustment));

    let waveform_zoom = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        MIN_WAVEFORM_ZOOM,
        MAX_WAVEFORM_ZOOM,
        0.01,
    );
    waveform_zoom.set_value(MIN_WAVEFORM_ZOOM);
    waveform_zoom.set_digits(1);
    waveform_zoom.set_draw_value(true);
    waveform_zoom.set_value_pos(gtk::PositionType::Right);
    waveform_zoom.set_hexpand(true);
    waveform_zoom.set_tooltip_text(Some(
        "Waveform zoom level; Ctrl+scroll over the waveform to zoom",
    ));
    waveform_zoom.set_format_value_func(|_, value| format!("{value:.1}×"));

    let zoom_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let zoom_label = gtk::Label::new(Some("Waveform zoom"));
    zoom_label.set_xalign(0.0);
    zoom_controls.append(&zoom_label);
    zoom_controls.append(&waveform_zoom);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.1);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    seek.set_sensitive(false);
    seek.set_tooltip_text(Some("Playback position"));

    let time_label = gtk::Label::new(Some("00:00 / 00:00"));
    time_label.set_width_chars(17);
    time_label.add_css_class("monospace");

    let play_button = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play/Pause at current position (K)")
        .sensitive(false)
        .build();
    let stop_button = gtk::Button::builder()
        .icon_name("media-playback-stop-symbolic")
        .tooltip_text("Stop")
        .sensitive(false)
        .build();

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    controls.set_halign(gtk::Align::Center);
    controls.append(&play_button);
    controls.append(&stop_button);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&waveform_frame);
    content.append(&waveform_scrollbar);
    content.append(&zoom_controls);
    content.append(&seek);
    content.append(&time_label);
    content.append(&controls);
    window.set_child(Some(&content));

    let state = Rc::new(UiState {
        window,
        player,
        play_button,
        stop_button,
        seek,
        time_label,
        waveform,
        waveform_adjustment,
        waveform_zoom,
        spinner,
        peaks: RefCell::new(Vec::new()),
        progress: Cell::new(0.0),
        playback_anchor: Cell::new(gst::ClockTime::ZERO),
        anchor_progress: Cell::new(0.0),
        playing: Cell::new(false),
        follow_playback: Cell::new(false),
        playhead_tick: RefCell::new(None),
        pending_zoom_focus: Cell::new(None),
        waveform_pointer: Cell::new(0.5),
        user_seeking: Cell::new(false),
        seek_change_serial: Cell::new(0),
        waveform_job: RefCell::new(None),
    });

    configure_waveform_drawing(&state);
    connect_open_button(&state, &open_button);
    connect_playback_controls(&state);
    connect_seeking(&state);
    connect_waveform_zoom(&state);
    connect_waveform_scroll_zoom(&state);
    connect_keyboard_controls(&state);
    let timer_source = Rc::new(RefCell::new(Some(start_ui_timer(&state))));
    let close_timer_source = Rc::clone(&timer_source);
    state.window.connect_close_request(move |_| {
        if let Some(source) = close_timer_source.borrow_mut().take() {
            source.remove();
        }
        glib::Propagation::Proceed
    });

    state.window.present();
}

fn configure_waveform_drawing(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state
        .waveform
        .set_draw_func(move |_area, context, width, height| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            draw_waveform(
                context,
                width as f64,
                height as f64,
                &state.peaks.borrow(),
                state.progress.get(),
                state.anchor_progress.get(),
                (
                    state.waveform_adjustment.value(),
                    state.waveform_adjustment.page_size(),
                ),
            );
        });
}

fn connect_waveform_zoom(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state.waveform_adjustment.connect_value_changed(move |_| {
        if let Some(state) = weak.upgrade() {
            state.waveform.queue_draw();
        }
    });

    let weak = Rc::downgrade(state);
    state.waveform_zoom.connect_value_changed(move |scale| {
        let Some(state) = weak.upgrade() else {
            return;
        };

        let zoom = scale.value().clamp(MIN_WAVEFORM_ZOOM, MAX_WAVEFORM_ZOOM);
        let (focus_time, focus_x) = state.pending_zoom_focus.take().unwrap_or_else(|| {
            let focus_time = if state.playing.get() {
                state.progress.get()
            } else {
                state.anchor_progress.get()
            };
            (focus_time, 0.5)
        });
        let (visible_start, visible_span) = zoomed_viewport(focus_time, focus_x, zoom);

        if !state.playing.get() {
            state.follow_playback.set(false);
        }
        state.waveform_adjustment.configure(
            visible_start,
            0.0,
            1.0,
            visible_span / 20.0,
            visible_span * 0.9,
            visible_span,
        );
        state.waveform.queue_draw();
    });
}

fn connect_waveform_scroll_zoom(state: &Rc<UiState>) {
    let pointer = gtk::EventControllerMotion::new();
    let weak = Rc::downgrade(state);
    pointer.connect_motion(move |_controller, x, _y| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let width = state.waveform.width().max(1) as f64;
        state.waveform_pointer.set((x / width).clamp(0.0, 1.0));
    });
    state.waveform.add_controller(pointer);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);

    let weak = Rc::downgrade(state);
    scroll.connect_scroll(move |controller, dx, dy| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let control_pressed = controller
            .current_event_state()
            .contains(gtk::gdk::ModifierType::CONTROL_MASK);

        if control_pressed {
            if dy.abs() < f64::EPSILON {
                return glib::Propagation::Stop;
            }

            let current = state.waveform_zoom.value();
            let next = zoom_after_scroll(current, dy);

            if (next - current).abs() > f64::EPSILON {
                let focus_x = state.waveform_pointer.get();
                let focus_time = timeline_fraction_at_x(
                    state.waveform_adjustment.value(),
                    state.waveform_adjustment.page_size(),
                    focus_x,
                    1.0,
                );
                state
                    .pending_zoom_focus
                    .set(Some((focus_time.clamp(0.0, 1.0), focus_x)));
                state.waveform_zoom.set_value(next);
            }
            return glib::Propagation::Stop;
        }

        let adjustment = &state.waveform_adjustment;
        let visible_span = adjustment.page_size();
        if visible_span >= 1.0 - f64::EPSILON {
            return glib::Propagation::Proceed;
        }

        let delta = if dx.abs() > dy.abs() { dx } else { dy };
        let visible_start = pan_viewport(adjustment.value(), visible_span, delta);
        state.follow_playback.set(false);
        adjustment.set_value(visible_start);
        glib::Propagation::Stop
    });
    state.waveform.add_controller(scroll);
}

fn connect_open_button(state: &Rc<UiState>, open_button: &gtk::Button) {
    let weak = Rc::downgrade(state);
    open_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };

        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Audio files (MP3, FLAC, Opus, Ogg)"));
        for pattern in [
            "*.mp3", "*.MP3", "*.flac", "*.FLAC", "*.opus", "*.OPUS", "*.ogg", "*.OGG",
        ] {
            filter.add_pattern(pattern);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Open Audio File")
            .accept_label("Open")
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .build();

        let weak = Rc::downgrade(&state);
        dialog.open(
            Some(&state.window),
            None::<&gio::Cancellable>,
            move |result| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(file) => match file.path() {
                        Some(path) => open_file(&state, &path),
                        None => show_error(
                            &state.window,
                            "Could not open the audio file",
                            "The selected item is not a local file.",
                        ),
                    },
                    Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {}
                    Err(error) => show_error(
                        &state.window,
                        "Could not open the file chooser",
                        &error.to_string(),
                    ),
                }
            },
        );
    });
}

fn connect_playback_controls(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state.play_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        toggle_current_playback(&state);
    });

    let weak = Rc::downgrade(state);
    state.stop_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };

        if let Err(error) = state.player.stop() {
            show_error(&state.window, "Could not stop playback", &error.to_string());
        } else {
            set_playing(&state, false);
            set_playback_anchor(&state, gst::ClockTime::ZERO, state.player.duration());
            update_position(&state, gst::ClockTime::ZERO, state.player.duration());
        }
    });
}

fn connect_keyboard_controls(state: &Rc<UiState>) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let weak = Rc::downgrade(state);
    controller.connect_key_pressed(move |_controller, key, _keycode, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let command_modifiers = gtk::gdk::ModifierType::CONTROL_MASK
            | gtk::gdk::ModifierType::ALT_MASK
            | gtk::gdk::ModifierType::SUPER_MASK
            | gtk::gdk::ModifierType::META_MASK;
        if modifiers.intersects(command_modifiers) {
            return glib::Propagation::Proceed;
        }

        match key {
            gtk::gdk::Key::k | gtk::gdk::Key::K => toggle_current_playback(&state),
            gtk::gdk::Key::space => play_from_anchor(&state, true),
            gtk::gdk::Key::p | gtk::gdk::Key::P => play_from_anchor(&state, false),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    state.window.add_controller(controller);
}

fn toggle_current_playback(state: &Rc<UiState>) {
    if !state.player.is_loaded() {
        return;
    }

    let should_play = !state.playing.get();
    let result = if should_play {
        state.player.play()
    } else {
        state.player.pause()
    };
    match result {
        Ok(()) => {
            if should_play {
                state.follow_playback.set(true);
            }
            set_playing(state, should_play);
        }
        Err(error) => show_error(&state.window, "Playback failed", &error.to_string()),
    }
}

fn play_from_anchor(state: &Rc<UiState>, toggle_pause: bool) {
    if !state.player.is_loaded() {
        return;
    }
    if toggle_pause && state.playing.get() {
        match state.player.pause() {
            Ok(()) => set_playing(state, false),
            Err(error) => show_error(&state.window, "Playback failed", &error.to_string()),
        }
        return;
    }

    let position = state.playback_anchor.get();
    let result = state
        .player
        .seek(position)
        .and_then(|()| state.player.play());
    match result {
        Ok(()) => {
            state.follow_playback.set(true);
            set_playing(state, true);
            update_position(state, position, state.player.duration());
        }
        Err(error) => show_error(&state.window, "Playback failed", &error.to_string()),
    }
}

fn connect_seeking(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state
        .seek
        .connect_change_value(move |_scale, _scroll, value| {
            let Some(state) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };

            state.user_seeking.set(true);
            let serial = state.seek_change_serial.get().wrapping_add(1);
            state.seek_change_serial.set(serial);

            let position = gst::ClockTime::from_nseconds((value.max(0.0) * 1e9) as u64);
            if let Err(error) = state.player.seek(position) {
                show_error(&state.window, "Could not seek", &error.to_string());
            } else {
                set_playback_anchor(&state, position, state.player.duration());
                update_time_label(&state.time_label, Some(position), state.player.duration());
            }

            let weak = Rc::downgrade(&state);
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                if let Some(state) = weak.upgrade()
                    && state.seek_change_serial.get() == serial
                {
                    state.user_seeking.set(false);
                }
            });

            glib::Propagation::Proceed
        });

    let waveform_click = gtk::GestureClick::new();
    let weak = Rc::downgrade(state);
    waveform_click.connect_pressed(move |_gesture, _, x, _| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let width = state.waveform.width().max(1) as f64;
        if let Some(duration) = state.player.duration() {
            let fraction = timeline_fraction_at_x(
                state.waveform_adjustment.value(),
                state.waveform_adjustment.page_size(),
                x,
                width,
            );
            let position = gst::ClockTime::from_nseconds(
                (duration.nseconds() as f64 * fraction).round() as u64,
            );
            if let Err(error) = state.player.seek(position) {
                show_error(&state.window, "Could not seek", &error.to_string());
            } else {
                set_playback_anchor(&state, position, Some(duration));
                update_position(&state, position, Some(duration));
            }
        }
    });
    state.waveform.add_controller(waveform_click);
}

fn start_ui_timer(state: &Rc<UiState>) -> glib::SourceId {
    let state = Rc::clone(state);
    glib::timeout_add_local(
        std::time::Duration::from_millis(UPDATE_INTERVAL_MS),
        move || {
            for event in state.player.poll_events() {
                handle_player_event(&state, event);
            }

            if state.player.is_loaded() && !state.playing.get() {
                let position = state.player.position().unwrap_or(gst::ClockTime::ZERO);
                let duration = state.player.duration();
                update_position(&state, position, duration);
            }

            poll_waveform_job(&state);
            glib::ControlFlow::Continue
        },
    )
}

fn open_file(state: &Rc<UiState>, path: &Path) {
    match state.player.open(path) {
        Ok(()) => {
            set_playing(state, false);
            state.seek.set_sensitive(true);
            state.stop_button.set_sensitive(true);
            state.play_button.set_sensitive(true);
            state.peaks.borrow_mut().clear();
            state.progress.set(0.0);
            set_playback_anchor(state, gst::ClockTime::ZERO, None);
            state.waveform_adjustment.set_value(0.0);
            state.waveform.queue_draw();
            update_position(state, gst::ClockTime::ZERO, None);

            let uri = gio::File::for_path(path).uri().to_string();
            state.spinner.set_visible(true);
            state.spinner.start();
            state.waveform_job.replace(Some(WaveformJob::start(uri)));
        }
        Err(error) => show_error(
            &state.window,
            "Could not open the audio file",
            &error.to_string(),
        ),
    }
}

fn handle_player_event(state: &Rc<UiState>, event: PlayerEvent) {
    match event {
        PlayerEvent::EndOfStream => {
            if let Err(error) = state.player.stop() {
                log::warn!("could not reset after end of stream: {error:#}");
            }
            set_playing(state, false);
            update_position(state, gst::ClockTime::ZERO, state.player.duration());
        }
        PlayerEvent::Error(details) => {
            set_playing(state, false);
            show_error(&state.window, "Audio playback error", &details);
        }
        PlayerEvent::StateChanged(gst::State::Playing) => set_playing(state, true),
        PlayerEvent::StateChanged(gst::State::Paused | gst::State::Ready) => {
            set_playing(state, false);
        }
        PlayerEvent::StateChanged(_) | PlayerEvent::DurationChanged => {}
    }
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

fn update_position(state: &UiState, position: gst::ClockTime, duration: Option<gst::ClockTime>) {
    if let Some(duration) = duration {
        let precise_duration_seconds = duration.nseconds() as f64 / 1e9;
        state.seek.set_range(0.0, precise_duration_seconds.max(1.0));
        if !state.user_seeking.get() {
            state.seek.set_value(position.nseconds() as f64 / 1e9);
        }
        let progress = if duration.is_zero() {
            0.0
        } else {
            position.nseconds() as f64 / duration.nseconds() as f64
        };
        state.progress.set(progress.clamp(0.0, 1.0));
        state.waveform.queue_draw();
        if state.playing.get() {
            keep_playback_cursor_visible(state);
        }
    }
    update_time_label(&state.time_label, Some(position), duration);
}

fn set_playback_anchor(
    state: &UiState,
    position: gst::ClockTime,
    duration: Option<gst::ClockTime>,
) {
    state.playback_anchor.set(position);
    let progress = duration
        .filter(|duration| !duration.is_zero())
        .map(|duration| position.nseconds() as f64 / duration.nseconds() as f64)
        .unwrap_or(0.0);
    state.anchor_progress.set(progress.clamp(0.0, 1.0));
    state.waveform.queue_draw();
}

fn zoomed_viewport(focus_time: f64, focus_x: f64, zoom: f64) -> (f64, f64) {
    let visible_span = 1.0 / zoom.clamp(MIN_WAVEFORM_ZOOM, MAX_WAVEFORM_ZOOM);
    let maximum = (1.0 - visible_span).max(0.0);
    let visible_start =
        (focus_time.clamp(0.0, 1.0) - focus_x.clamp(0.0, 1.0) * visible_span).clamp(0.0, maximum);
    (visible_start, visible_span)
}

fn zoom_after_scroll(current_zoom: f64, delta_y: f64) -> f64 {
    (current_zoom * 2.0_f64.powf(-delta_y / ZOOM_OCTAVE_SCROLL_UNITS))
        .clamp(MIN_WAVEFORM_ZOOM, MAX_WAVEFORM_ZOOM)
}

fn pan_viewport(visible_start: f64, visible_span: f64, delta: f64) -> f64 {
    let maximum = (1.0 - visible_span).max(0.0);
    (visible_start + delta * visible_span * PAN_FRACTION_PER_SCROLL_UNIT).clamp(0.0, maximum)
}

fn timeline_fraction_at_x(visible_start: f64, visible_span: f64, x: f64, width: f64) -> f64 {
    let screen_fraction = (x / width.max(1.0)).clamp(0.0, 1.0);
    (visible_start + screen_fraction * visible_span).clamp(0.0, 1.0)
}

fn keep_playback_cursor_visible(state: &UiState) {
    if !state.follow_playback.get() || state.waveform_zoom.value() <= MIN_WAVEFORM_ZOOM {
        return;
    }

    let adjustment = &state.waveform_adjustment;
    let page_size = adjustment.page_size().clamp(0.0, 1.0);
    if page_size <= 0.0 {
        return;
    }

    let cursor = state.progress.get();
    let margin = page_size * 0.15;
    let visible_start = adjustment.value();
    let visible_end = visible_start + page_size;
    let requested = if cursor < visible_start + margin {
        Some(cursor - margin)
    } else if cursor > visible_end - margin {
        Some(cursor - page_size + margin)
    } else {
        None
    };

    if let Some(value) = requested {
        let maximum = (1.0 - page_size).max(0.0);
        adjustment.set_value(value.clamp(0.0, maximum));
    }
}

fn update_time_label(
    label: &gtk::Label,
    position: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
) {
    label.set_text(&format!(
        "{} / {}",
        format_time(position),
        format_time(duration)
    ));
}

fn format_time(time: Option<gst::ClockTime>) -> String {
    let seconds = time.map(|time| time.seconds()).unwrap_or(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn set_playing(state: &Rc<UiState>, playing: bool) {
    state.playing.set(playing);
    if playing {
        state
            .play_button
            .set_icon_name("media-playback-pause-symbolic");
        state
            .play_button
            .set_tooltip_text(Some("Pause at current position (K)"));

        if state.playhead_tick.borrow().is_none() {
            let weak = Rc::downgrade(state);
            let tick = state
                .waveform
                .add_tick_callback(move |_waveform, _frame_clock| {
                    let Some(state) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    if !state.playing.get() {
                        return glib::ControlFlow::Break;
                    }

                    if let Some(position) = state.player.position() {
                        update_position(&state, position, state.player.duration());
                    }
                    glib::ControlFlow::Continue
                });
            state.playhead_tick.replace(Some(tick));
        }
    } else {
        if let Some(tick) = state.playhead_tick.borrow_mut().take() {
            tick.remove();
        }
        state
            .play_button
            .set_icon_name("media-playback-start-symbolic");
        state
            .play_button
            .set_tooltip_text(Some("Play at current position (K)"));
    }
}

fn draw_waveform(
    context: &cairo::Context,
    width: f64,
    height: f64,
    peaks: &[f32],
    progress: f64,
    anchor_progress: f64,
    viewport: (f64, f64),
) {
    let (visible_start, visible_span) = viewport;
    let visible_span = visible_span.clamp(1.0 / MAX_WAVEFORM_ZOOM, 1.0);
    let visible_start = visible_start.clamp(0.0, (1.0 - visible_span).max(0.0));
    let visible_end = visible_start + visible_span;

    context.set_source_rgb(0.16, 0.16, 0.18);
    let _ = context.paint();

    let center = height / 2.0;
    context.set_source_rgba(1.0, 1.0, 1.0, 0.20);
    context.set_line_width(1.0);
    context.move_to(0.0, center.floor() + 0.5);
    context.line_to(width, center.floor() + 0.5);
    let _ = context.stroke();

    if peaks.is_empty() {
        return;
    }

    draw_peaks(
        context,
        width,
        height,
        peaks,
        visible_start,
        visible_span,
        (0.68, 0.68, 0.72),
    );

    context.save().ok();
    let cursor_x = ((progress - visible_start) / visible_span * width).clamp(0.0, width);
    context.rectangle(0.0, 0.0, cursor_x, height);
    context.clip();
    draw_peaks(
        context,
        width,
        height,
        peaks,
        visible_start,
        visible_span,
        (0.22, 0.62, 0.96),
    );
    context.restore().ok();

    if (visible_start..=visible_end).contains(&anchor_progress) {
        let anchor_x = (anchor_progress - visible_start) / visible_span * width;
        context.set_source_rgb(1.0, 0.67, 0.18);
        context.set_line_width(2.5);
        context.move_to(anchor_x, 0.0);
        context.line_to(anchor_x, height);
        let _ = context.stroke();
    }

    if (visible_start..=visible_end).contains(&progress) {
        let cursor_x = (progress - visible_start) / visible_span * width;
        context.set_source_rgb(0.95, 0.95, 0.98);
        context.set_line_width(1.5);
        context.move_to(cursor_x, 0.0);
        context.line_to(cursor_x, height);
        let _ = context.stroke();
    }
}

fn draw_peaks(
    context: &cairo::Context,
    width: f64,
    height: f64,
    peaks: &[f32],
    visible_start: f64,
    visible_span: f64,
    color: (f64, f64, f64),
) {
    if width <= 0.0 || peaks.is_empty() {
        return;
    }

    let center = height / 2.0;
    let amplitude = (center - 8.0).max(1.0);
    let (clip_start, _, clip_end, _) = context.clip_extents().unwrap_or((0.0, 0.0, width, height));
    let clip_start = clip_start.clamp(0.0, width);
    let clip_end = clip_end.clamp(0.0, width);
    if clip_end <= clip_start {
        return;
    }

    context.set_source_rgb(color.0, color.1, color.2);
    context.set_line_width(1.0);

    let visible_peak_count = peaks.len() as f64 * visible_span;
    if visible_peak_count >= width {
        let first_column = clip_start.floor().max(0.0) as usize;
        let last_column = clip_end.ceil().min(width.ceil()) as usize;

        for column in first_column..last_column {
            let start_fraction = visible_start + column as f64 / width * visible_span;
            let end_fraction = visible_start + (column + 1) as f64 / width * visible_span;
            let start = (start_fraction * peaks.len() as f64)
                .floor()
                .clamp(0.0, (peaks.len() - 1) as f64) as usize;
            let end = (end_fraction * peaks.len() as f64)
                .ceil()
                .max((start + 1) as f64)
                .min(peaks.len() as f64) as usize;
            let peak = peaks[start..end].iter().copied().fold(0.0_f32, f32::max) as f64;
            let x = column as f64 + 0.5;
            context.move_to(x, center - peak * amplitude);
            context.line_to(x, center + peak * amplitude);
        }
    } else {
        let clip_time_start = visible_start + clip_start / width * visible_span;
        let clip_time_end = visible_start + clip_end / width * visible_span;
        let first_peak = (clip_time_start * peaks.len() as f64).floor().max(0.0) as usize;
        let first_peak = first_peak.saturating_sub(1);
        let last_peak = ((clip_time_end * peaks.len() as f64).ceil() as usize + 1).min(peaks.len());

        for (index, peak) in peaks.iter().enumerate().take(last_peak).skip(first_peak) {
            let peak_time = (index as f64 + 0.5) / peaks.len() as f64;
            let x = (peak_time - visible_start) / visible_span * width;
            let peak = f64::from(*peak);
            context.move_to(x, center - peak * amplitude);
            context.line_to(x, center + peak * amplitude);
        }
    }
    let _ = context.stroke();
}

fn show_error(parent: &impl IsA<gtk::Window>, title: &str, details: &str) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(details)
        .build();
    dialog.show(Some(parent));
}

fn show_startup_error(application: &gtk::Application, details: &str) {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Transcription MVP")
        .default_width(420)
        .default_height(120)
        .build();
    let label = gtk::Label::new(Some(&format!(
        "Could not initialize audio playback:\n\n{details}"
    )));
    label.set_wrap(true);
    label.set_margin_top(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label.set_margin_end(24);
    window.set_child(Some(&label));
    window.present();
}

#[cfg(test)]
mod tests {
    use super::{pan_viewport, timeline_fraction_at_x, zoom_after_scroll, zoomed_viewport};

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn slider_zoom_centers_the_anchor() {
        let (start, span) = zoomed_viewport(0.63, 0.5, 10.0);
        assert_close(start, 0.58);
        assert_close(span, 0.1);
        assert_close(timeline_fraction_at_x(start, span, 0.5, 1.0), 0.63);
    }

    #[test]
    fn pointer_zoom_preserves_the_time_under_the_pointer() {
        let old_start = 0.25;
        let old_span = 0.2;
        let pointer_x = 0.8;
        let focus = timeline_fraction_at_x(old_start, old_span, pointer_x, 1.0);
        let (new_start, new_span) = zoomed_viewport(focus, pointer_x, 20.0);

        assert_close(
            timeline_fraction_at_x(new_start, new_span, pointer_x, 1.0),
            focus,
        );
    }

    #[test]
    fn zoom_clamps_cleanly_at_the_timeline_edges() {
        let (start, span) = zoomed_viewport(0.02, 0.5, 10.0);
        assert_close(start, 0.0);
        assert_close(span, 0.1);

        let (start, _) = zoomed_viewport(0.98, 0.5, 10.0);
        assert_close(start, 0.9);
    }

    #[test]
    fn scroll_zoom_is_continuous_and_multiplicative() {
        assert_close(zoom_after_scroll(5.0, -0.5), 5.452_538_663_326_289);
        assert_close(zoom_after_scroll(5.0, -4.0), 10.0);
        assert_close(zoom_after_scroll(5.0, 4.0), 2.5);
    }

    #[test]
    fn two_finger_scroll_pans_by_a_fraction_of_the_view() {
        assert_close(pan_viewport(0.25, 0.2, 1.0), 0.27);
        assert_close(pan_viewport(0.0, 0.2, -1.0), 0.0);
        assert_close(pan_viewport(0.8, 0.2, 1.0), 0.8);
    }
}
