use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gstreamer as gst;
use gtk::{gio, glib, prelude::*};

use crate::{
    player::Player,
    preferences::{Preferences, PreferencesStore},
    recent::RecentStore,
    waveform::render::{MAX_WAVEFORM_ZOOM, WaveformView, draw_waveform},
};

use super::{
    MIN_WAVEFORM_ZOOM, UiState,
    file_ui::{connect_open_button, connect_recent_action, rebuild_recent_menu},
    loop_ui::connect_waveform_loop_gesture,
    marker_ui::connect_marker_controls,
    playback_ui::{
        connect_playback_controls, connect_seek_scale, connect_speed_control,
        connect_volume_control, start_ui_timer,
    },
    settings_ui::{connect_keyboard_controls, connect_settings_button},
    waveform_interaction::{
        connect_waveform_pinch_zoom, connect_waveform_scroll_zoom, connect_waveform_zoom,
    },
};

pub(super) fn build_ui(application: &gtk::Application) {
    install_css();

    let player = match Player::new() {
        Ok(player) => player,
        Err(error) => {
            show_startup_error(application, &error.to_string());
            return;
        }
    };
    let preferences_store = PreferencesStore::new();
    let preferences = preferences_store.load().unwrap_or_else(|error| {
        log::warn!("could not load settings: {error:#}");
        Preferences::default()
    });
    let recent_store = RecentStore::new();
    let recent_files = recent_store.load().unwrap_or_else(|error| {
        log::warn!("could not load recent files: {error:#}");
        Vec::new()
    });

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Transkribera")
        .default_width(720)
        .default_height(460)
        .width_request(360)
        .height_request(340)
        .build();

    let header = gtk::HeaderBar::new();
    let open_button = gtk::Button::builder()
        .label("Open")
        .tooltip_text("Open an audio file")
        .build();
    header.pack_start(&open_button);
    let recent_menu = gio::Menu::new();
    let recent_popover = gtk::PopoverMenu::from_model(Some(&recent_menu));
    let recent_button = gtk::MenuButton::builder()
        .label("Recent")
        .tooltip_text("Recently opened audio files")
        .popover(&recent_popover)
        .build();
    header.pack_start(&recent_button);
    let settings_button = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .build();
    header.pack_end(&settings_button);
    window.set_titlebar(Some(&header));

    let waveform = gtk::DrawingArea::builder()
        .content_width(600)
        .content_height(180)
        .hexpand(true)
        .vexpand(true)
        .build();
    waveform.set_tooltip_text(Some(
        "Click to seek; two-finger scroll to pan; pinch or Ctrl+scroll to zoom",
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
        "Waveform zoom level; pinch or Ctrl+scroll over the waveform to zoom",
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
        .tooltip_text("Play/Pause at current position")
        .sensitive(false)
        .build();
    let beginning_button = gtk::Button::builder()
        .icon_name("go-first-symbolic")
        .tooltip_text("Go to beginning")
        .sensitive(false)
        .build();
    let stop_button = gtk::Button::builder()
        .icon_name("media-playback-stop-symbolic")
        .tooltip_text("Stop")
        .sensitive(false)
        .build();
    let end_button = gtk::Button::builder()
        .icon_name("go-last-symbolic")
        .tooltip_text("Go to end")
        .sensitive(false)
        .build();

    let volume = gtk::ScaleButton::new(
        0.0,
        1.0,
        0.02,
        &[
            "audio-volume-muted-symbolic",
            "audio-volume-low-symbolic",
            "audio-volume-medium-symbolic",
            "audio-volume-high-symbolic",
        ],
    );
    volume.set_value(1.0);
    volume.set_tooltip_text(Some("Volume"));

    let speed_label = gtk::Label::new(Some("Speed: 1.00×"));
    let speed = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.25, 1.50, 0.05);
    speed.set_value(1.0);
    speed.set_draw_value(false);
    speed.set_width_request(130);
    speed.set_tooltip_text(Some("Playback speed with pitch preservation"));
    let tempo_bypass = gtk::CheckButton::with_label("Bypass");
    tempo_bypass.set_tooltip_text(Some(
        "Play decoded audio directly without phase-vocoder processing",
    ));
    let speed_control = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    speed_control.append(&speed_label);
    speed_control.append(&speed);
    speed_control.append(&tempo_bypass);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    controls.set_halign(gtk::Align::Center);
    controls.append(&beginning_button);
    controls.append(&play_button);
    controls.append(&stop_button);
    controls.append(&end_button);
    controls.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    controls.append(&volume);
    controls.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    controls.append(&speed_control);

    let add_marker_button = gtk::Button::builder()
        .label("Add marker")
        .tooltip_text("Add a marker at the playhead")
        .sensitive(false)
        .build();
    let marker_list = gtk::ListBox::new();
    marker_list.set_selection_mode(gtk::SelectionMode::None);
    marker_list.add_css_class("boxed-list");
    let marker_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(64)
        .vexpand(true)
        .child(&marker_list)
        .build();
    let marker_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    add_marker_button.set_halign(gtk::Align::End);
    marker_content.append(&add_marker_button);
    marker_content.append(&marker_scroll);
    let marker_expander = gtk::Expander::builder()
        .label("Markers")
        .expanded(true)
        .hexpand(true)
        .vexpand(true)
        .child(&marker_content)
        .build();

    let loop_list = gtk::ListBox::new();
    loop_list.set_selection_mode(gtk::SelectionMode::None);
    loop_list.add_css_class("boxed-list");
    let loop_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(64)
        .vexpand(true)
        .child(&loop_list)
        .build();
    let loop_help = gtk::Label::new(Some("Drag on the waveform to create a loop"));
    loop_help.set_xalign(0.0);
    loop_help.set_wrap(true);
    loop_help.add_css_class("dim-label");
    let loop_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    loop_content.append(&loop_help);
    loop_content.append(&loop_scroll);
    let loop_expander = gtk::Expander::builder()
        .label("Loops")
        .expanded(true)
        .hexpand(true)
        .vexpand(true)
        .child(&loop_content)
        .build();

    let annotation_boxes = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    annotation_boxes.set_homogeneous(true);
    annotation_boxes.append(&marker_expander);
    annotation_boxes.append(&loop_expander);

    let playback_content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    playback_content.append(&waveform_frame);
    playback_content.append(&waveform_scrollbar);
    playback_content.append(&zoom_controls);
    playback_content.append(&seek);
    playback_content.append(&time_label);
    playback_content.append(&controls);

    let split = gtk::Paned::new(gtk::Orientation::Vertical);
    split.add_css_class("marker-split");
    split.set_wide_handle(true);
    split.set_resize_start_child(true);
    split.set_resize_end_child(true);
    split.set_shrink_start_child(false);
    split.set_shrink_end_child(false);
    split.set_position(315);
    split.set_start_child(Some(&playback_content));
    split.set_end_child(Some(&annotation_boxes));
    split.set_margin_top(12);
    split.set_margin_bottom(12);
    split.set_margin_start(12);
    split.set_margin_end(12);
    window.set_child(Some(&split));

    let state = Rc::new(UiState {
        window,
        player,
        beginning_button,
        play_button,
        stop_button,
        end_button,
        add_marker_button,
        marker_list,
        loop_list,
        recent_button,
        recent_menu,
        settings_window: RefCell::new(None),
        seek,
        time_label,
        waveform,
        waveform_adjustment,
        waveform_zoom,
        spinner,
        duration: Cell::new(None),
        peaks: RefCell::new(Vec::new()),
        markers: RefCell::new(Vec::new()),
        marker_store: RefCell::new(None),
        loops: RefCell::new(Vec::new()),
        loop_store: RefCell::new(None),
        active_loop: Cell::new(None),
        loop_drag: RefCell::new(None),
        loop_preview: Cell::new(None),
        preferences_store,
        prompt_for_marker_name: Cell::new(preferences.prompt_for_marker_name),
        key_bindings: RefCell::new(preferences.key_bindings),
        recent_store,
        recent_files: RefCell::new(recent_files),
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
    connect_recent_action(&state);
    rebuild_recent_menu(&state);
    connect_open_button(&state, &open_button);
    connect_settings_button(&state, &settings_button);
    connect_playback_controls(&state);
    connect_marker_controls(&state);
    connect_volume_control(&state, &volume);
    connect_speed_control(&state, &speed, &speed_label, &tempo_bypass);
    connect_seek_scale(&state);
    connect_waveform_loop_gesture(&state);
    connect_waveform_zoom(&state);
    connect_waveform_scroll_zoom(&state);
    connect_waveform_pinch_zoom(&state);
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

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "
        paned.marker-split > separator {
            margin-top: 6px;
            margin-bottom: 6px;
            border-top: 0px solid alpha(@theme_fg_color, 0.24);
        }
        ",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn configure_waveform_drawing(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state
        .waveform
        .set_draw_func(move |_area, context, width, height| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let peaks = state.peaks.borrow();
            let markers = state.markers.borrow();
            let loops = state.loops.borrow();
            draw_waveform(
                context,
                width as f64,
                height as f64,
                WaveformView {
                    peaks: &peaks,
                    markers: &markers,
                    loops: &loops,
                    active_loop: state.active_loop.get(),
                    loop_preview: state.loop_preview.get(),
                    duration_ns: state.duration.get().map(|duration| duration.nseconds()),
                    progress: state.progress.get(),
                    anchor_progress: state.anchor_progress.get(),
                    visible_start: state.waveform_adjustment.value(),
                    visible_span: state.waveform_adjustment.page_size(),
                },
            );
        });
}

fn show_startup_error(application: &gtk::Application, details: &str) {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Transkribera")
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
