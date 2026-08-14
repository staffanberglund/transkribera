mod navigation;
mod waveform_interaction;

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

use gstreamer as gst;
use gtk::{gio, glib, prelude::*};

use crate::{
    loops::{LoopRegion, LoopStore},
    markers::{Marker, MarkerStore},
    player::{Player, PlayerEvent},
    preferences::{Preferences, PreferencesStore},
    recent::{RecentStore, record as record_recent},
    shortcuts::{Command, KeyBinding, accelerator_for_event, default_key_bindings},
    waveform::{
        WaveformJob,
        render::{MAX_WAVEFORM_ZOOM, WaveformView, draw_waveform},
    },
};

use self::{
    navigation::{
        format_marker_time, navigation_position, seek_relative, seek_to_position,
        set_playback_anchor, update_position, update_time_label,
    },
    waveform_interaction::{
        connect_waveform_pinch_zoom, connect_waveform_scroll_zoom, connect_waveform_zoom,
        waveform_x_to_ns,
    },
};

const APP_ID: &str = "io.github.staffanberglund.transkribera";
const UPDATE_INTERVAL_MS: u64 = 150;
const MIN_WAVEFORM_ZOOM: f64 = 1.0;
const MARKER_JUMP_TOLERANCE_NS: u64 = 50_000_000;
const LOOP_HANDLE_HIT_RADIUS_PX: f64 = 8.0;
const LOOP_DRAG_THRESHOLD_PX: f64 = 3.0;

pub fn run() {
    let application = gtk::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
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

fn build_ui(application: &gtk::Application) {
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
    connect_seeking(&state);
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

fn rebuild_recent_menu(state: &Rc<UiState>) {
    state.recent_menu.remove_all();

    let files = state.recent_files.borrow().clone();
    state.recent_button.set_sensitive(!files.is_empty());
    for path in files {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let label = escape_menu_label(&name);
        let item = gio::MenuItem::new(Some(&label), None);
        let target = path.to_string_lossy().into_owned().to_variant();
        item.set_action_and_target_value(Some("win.open-recent"), Some(&target));
        state.recent_menu.append_item(&item);
    }
}

fn escape_menu_label(label: &str) -> String {
    label.replace('_', "__")
}

fn connect_recent_action(state: &Rc<UiState>) {
    let action = gio::SimpleAction::new("open-recent", Some(&String::static_variant_type()));
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_action, parameter| {
        let Some(path) = parameter.and_then(|value| value.get::<String>()) else {
            return;
        };
        let Some(state) = weak.upgrade() else {
            return;
        };
        open_file(&state, Path::new(&path));
    });
    state.window.add_action(&action);
}

fn record_recent_file(state: &Rc<UiState>, path: &Path) {
    let mut files = state.recent_files.borrow_mut();
    record_recent(&mut files, path);
    if let Err(error) = state.recent_store.save(&files) {
        drop(files);
        show_error(
            &state.window,
            "Could not save recent files",
            &error.to_string(),
        );
        return;
    }
    drop(files);
    rebuild_recent_menu(state);
}

fn connect_settings_button(state: &Rc<UiState>, settings_button: &gtk::Button) {
    let weak = Rc::downgrade(state);
    settings_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if let Some(window) = state.settings_window.borrow().as_ref() {
            window.present();
            return;
        }

        let window = gtk::Window::builder()
            .title("Settings")
            .transient_for(&state.window)
            .default_width(620)
            .default_height(620)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let marker_row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        let marker_text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        marker_text.set_hexpand(true);
        let title = gtk::Label::new(Some("Prompt for marker name"));
        title.set_xalign(0.0);
        let description = gtk::Label::new(Some(
            "When disabled, new markers receive an automatic name.",
        ));
        description.set_xalign(0.0);
        description.set_wrap(true);
        description.add_css_class("dim-label");
        marker_text.append(&title);
        marker_text.append(&description);
        let prompt_switch = gtk::Switch::new();
        prompt_switch.set_valign(gtk::Align::Center);
        prompt_switch.set_active(state.prompt_for_marker_name.get());
        marker_row.append(&marker_text);
        marker_row.append(&prompt_switch);
        content.append(&marker_row);

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let shortcut_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let shortcut_text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        shortcut_text.set_hexpand(true);
        let shortcut_title = gtk::Label::new(Some("Keyboard shortcuts"));
        shortcut_title.set_xalign(0.0);
        shortcut_title.add_css_class("title-4");
        let shortcut_description = gtk::Label::new(Some(
            "Edit a shortcut or add another key combination for an application command.",
        ));
        shortcut_description.set_xalign(0.0);
        shortcut_description.set_wrap(true);
        shortcut_description.add_css_class("dim-label");
        shortcut_text.append(&shortcut_title);
        shortcut_text.append(&shortcut_description);
        let add_shortcut = gtk::Button::with_label("Add shortcut");
        add_shortcut.set_valign(gtk::Align::Center);
        shortcut_header.append(&shortcut_text);
        shortcut_header.append(&add_shortcut);
        content.append(&shortcut_header);

        let shortcut_list = gtk::ListBox::new();
        shortcut_list.set_selection_mode(gtk::SelectionMode::None);
        shortcut_list.add_css_class("boxed-list");
        let shortcut_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(280)
            .vexpand(true)
            .child(&shortcut_list)
            .build();
        content.append(&shortcut_scroll);

        let reset_shortcuts = gtk::Button::with_label("Restore default shortcuts");
        reset_shortcuts.set_halign(gtk::Align::Start);
        content.append(&reset_shortcuts);
        window.set_child(Some(&content));

        rebuild_shortcut_list(&state, &shortcut_list);

        let weak = Rc::downgrade(&state);
        let weak_list = shortcut_list.downgrade();
        add_shortcut.connect_clicked(move |_| {
            if let (Some(state), Some(list)) = (weak.upgrade(), weak_list.upgrade()) {
                show_shortcut_editor(&state, &list, None);
            }
        });

        let weak = Rc::downgrade(&state);
        let weak_list = shortcut_list.downgrade();
        let weak_window = window.downgrade();
        reset_shortcuts.connect_clicked(move |_| {
            let (Some(state), Some(list)) = (weak.upgrade(), weak_list.upgrade()) else {
                return;
            };
            state.key_bindings.replace(default_key_bindings());
            if let Err(error) = save_preferences(&state)
                && let Some(window) = weak_window.upgrade()
            {
                show_error(&window, "Could not save settings", &error.to_string());
            }
            rebuild_shortcut_list(&state, &list);
        });

        let weak = Rc::downgrade(&state);
        let weak_window = window.downgrade();
        prompt_switch.connect_active_notify(move |switch| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let prompt_for_marker_name = switch.is_active();
            state.prompt_for_marker_name.set(prompt_for_marker_name);
            if let Err(error) = save_preferences(&state)
                && let Some(window) = weak_window.upgrade()
            {
                show_error(&window, "Could not save settings", &error.to_string());
            }
        });

        let weak = Rc::downgrade(&state);
        window.connect_close_request(move |_| {
            if let Some(state) = weak.upgrade() {
                state.settings_window.borrow_mut().take();
            }
            glib::Propagation::Proceed
        });
        state.settings_window.replace(Some(window.clone()));
        window.present();
    });
}

fn save_preferences(state: &UiState) -> anyhow::Result<()> {
    state.preferences_store.save(&Preferences {
        prompt_for_marker_name: state.prompt_for_marker_name.get(),
        key_bindings: state.key_bindings.borrow().clone(),
    })
}

fn rebuild_shortcut_list(state: &Rc<UiState>, list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    for (index, binding) in state.key_bindings.borrow().iter().enumerate() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(9);
        row.set_margin_end(9);
        let command = gtk::Label::new(Some(binding.command.label()));
        command.set_xalign(0.0);
        command.set_hexpand(true);
        let edit = gtk::Button::with_label(&binding.display_label());
        edit.set_tooltip_text(Some("Change this keyboard shortcut"));
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove this keyboard shortcut")
            .build();

        let weak = Rc::downgrade(state);
        let weak_list = list.downgrade();
        edit.connect_clicked(move |_| {
            if let (Some(state), Some(list)) = (weak.upgrade(), weak_list.upgrade()) {
                show_shortcut_editor(&state, &list, Some(index));
            }
        });

        let weak = Rc::downgrade(state);
        let weak_list = list.downgrade();
        delete.connect_clicked(move |_| {
            let (Some(state), Some(list)) = (weak.upgrade(), weak_list.upgrade()) else {
                return;
            };
            if index < state.key_bindings.borrow().len() {
                state.key_bindings.borrow_mut().remove(index);
                if let Err(error) = save_preferences(&state) {
                    show_error(&state.window, "Could not save settings", &error.to_string());
                }
                rebuild_shortcut_list(&state, &list);
            }
        });

        row.append(&command);
        row.append(&edit);
        row.append(&delete);
        list.append(&row);
    }
}

fn show_shortcut_editor(state: &Rc<UiState>, list: &gtk::ListBox, index: Option<usize>) {
    let existing = index.and_then(|index| state.key_bindings.borrow().get(index).cloned());
    let window = gtk::Window::builder()
        .title(if existing.is_some() {
            "Edit shortcut"
        } else {
            "Add shortcut"
        })
        .transient_for(&state.window)
        .modal(true)
        .default_width(440)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let command_label = gtk::Label::new(Some("Command"));
    command_label.set_xalign(0.0);
    let command_labels = Command::ALL.map(Command::label);
    let command_picker = gtk::DropDown::from_strings(&command_labels);
    let selected_command = existing
        .as_ref()
        .map(|binding| binding.command)
        .unwrap_or(Command::TogglePlayback);
    let selected = Command::ALL
        .iter()
        .position(|command| *command == selected_command)
        .unwrap_or(0) as u32;
    command_picker.set_selected(selected);

    let key_label = gtk::Label::new(Some("Keyboard shortcut"));
    key_label.set_xalign(0.0);
    let key_button = gtk::Button::with_label(
        &existing
            .as_ref()
            .map(KeyBinding::display_label)
            .unwrap_or_else(|| "Press a key combination".into()),
    );
    key_button.set_sensitive(false);
    key_button.add_css_class("suggested-action");
    let explanation = gtk::Label::new(Some(
        "Press the desired key combination now. Modifier-only input is ignored.",
    ));
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    explanation.add_css_class("dim-label");

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_sensitive(existing.is_some());
    buttons.append(&cancel);
    buttons.append(&save);

    content.append(&command_label);
    content.append(&command_picker);
    content.append(&key_label);
    content.append(&key_button);
    content.append(&explanation);
    content.append(&buttons);
    window.set_child(Some(&content));

    let accelerator = Rc::new(RefCell::new(
        existing.as_ref().map(|binding| binding.accelerator.clone()),
    ));
    // Capture the next complete key combination directly from this modal window.
    let controller = gtk::EventControllerKey::new();
    let captured = Rc::clone(&accelerator);
    let captured_button = key_button.clone();
    let captured_save = save.clone();
    controller.connect_key_pressed(move |_controller, key, _keycode, modifiers| {
        let Some(value) = accelerator_for_event(key, modifiers) else {
            return glib::Propagation::Stop;
        };
        let label = gtk::accelerator_parse(&value)
            .map(|(key, modifiers)| gtk::accelerator_get_label(key, modifiers).to_string())
            .unwrap_or_else(|| value.clone());
        captured.replace(Some(value));
        captured_button.set_label(&label);
        captured_save.set_sensitive(true);
        glib::Propagation::Stop
    });
    window.add_controller(controller);

    let weak_window = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });

    let weak = Rc::downgrade(state);
    let weak_list = list.downgrade();
    let weak_window = window.downgrade();
    save.connect_clicked(move |_| {
        let (Some(state), Some(list), Some(window), Some(accelerator)) = (
            weak.upgrade(),
            weak_list.upgrade(),
            weak_window.upgrade(),
            accelerator.borrow().clone(),
        ) else {
            return;
        };
        if state
            .key_bindings
            .borrow()
            .iter()
            .enumerate()
            .any(|(candidate, binding)| {
                candidate != index.unwrap_or(usize::MAX) && binding.accelerator == accelerator
            })
        {
            show_error(
                &window,
                "Shortcut already in use",
                "Choose a different key combination.",
            );
            return;
        }
        let command = Command::ALL
            .get(command_picker.selected() as usize)
            .copied()
            .unwrap_or(Command::TogglePlayback);
        let binding = KeyBinding::new(command, accelerator);
        if let Some(index) = index {
            if let Some(existing) = state.key_bindings.borrow_mut().get_mut(index) {
                *existing = binding;
            }
        } else {
            state.key_bindings.borrow_mut().push(binding);
        }
        if let Err(error) = save_preferences(&state) {
            show_error(&window, "Could not save settings", &error.to_string());
            return;
        }
        rebuild_shortcut_list(&state, &list);
        window.close();
    });
    window.present();
}

fn connect_playback_controls(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state.beginning_button.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            seek_to_position(&state, gst::ClockTime::ZERO);
        }
    });

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

    let weak = Rc::downgrade(state);
    state.end_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.playing.get() {
            if let Err(error) = state.player.pause() {
                show_error(
                    &state.window,
                    "Could not pause playback",
                    &error.to_string(),
                );
                return;
            }
            set_playing(&state, false);
        }
        if let Some(duration) = state.duration.get().or_else(|| state.player.duration()) {
            seek_to_position(&state, duration);
        }
    });
}

fn connect_volume_control(state: &Rc<UiState>, volume: &gtk::ScaleButton) {
    let weak = Rc::downgrade(state);
    volume.connect_value_changed(move |_button, value| {
        if let Some(state) = weak.upgrade() {
            state.player.set_volume(value);
        }
    });
}

fn connect_speed_control(
    state: &Rc<UiState>,
    speed: &gtk::Scale,
    label: &gtk::Label,
    bypass: &gtk::CheckButton,
) {
    let weak = Rc::downgrade(state);
    let speed_label = label.clone();
    let bypass_for_speed = bypass.clone();
    speed.connect_value_changed(move |scale| {
        let playback_speed = scale.value() as f32;
        if bypass_for_speed.is_active() {
            speed_label.set_text("Speed: bypassed");
        } else {
            speed_label.set_text(&format!("Speed: {playback_speed:.2}×"));
        }
        if !bypass_for_speed.is_active()
            && let Some(state) = weak.upgrade()
            && let Err(error) = state.player.set_playback_speed(playback_speed)
        {
            show_error(
                &state.window,
                "Could not change playback speed",
                &error.to_string(),
            );
        }
    });

    let weak = Rc::downgrade(state);
    let speed = speed.clone();
    let label = label.clone();
    bypass.connect_toggled(move |check| {
        let bypass = check.is_active();
        speed.set_sensitive(!bypass);
        if bypass {
            label.set_text("Speed: bypassed");
        } else {
            label.set_text(&format!("Speed: {:.2}×", speed.value()));
        }

        if let Some(state) = weak.upgrade()
            && let Err(error) = state.player.set_tempo_bypass(bypass)
        {
            show_error(
                &state.window,
                "Could not change tempo-processing mode",
                &error.to_string(),
            );
        }
    });
}

fn connect_marker_controls(state: &Rc<UiState>) {
    let weak = Rc::downgrade(state);
    state.add_marker_button.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let Some(position_ns) = current_marker_position(&state) else {
            return;
        };
        let name = next_generic_marker_name(&state.markers.borrow());
        if state.prompt_for_marker_name.get() {
            show_marker_name_dialog(&state, position_ns, name, false);
        } else {
            add_named_marker(&state, position_ns, name);
        }
    });
}

fn current_marker_position(state: &UiState) -> Option<u64> {
    let duration = state.duration.get().or_else(|| state.player.duration())?;
    Some(
        navigation_position(
            state.playing.get(),
            state.player.position(),
            state.playback_anchor.get(),
        )
        .min(duration)
        .nseconds(),
    )
}

fn next_generic_marker_name(markers: &[Marker]) -> String {
    let mut index = 1;
    loop {
        let name = format!("Marker {index}");
        if markers.iter().all(|marker| marker.name != name) {
            return name;
        }
        index += 1;
    }
}

fn show_marker_name_dialog(
    state: &Rc<UiState>,
    position_ns: u64,
    initial_name: String,
    renaming: bool,
) {
    let title = if renaming {
        "Rename marker"
    } else {
        "Add marker"
    };
    let action_label = if renaming { "Rename" } else { "Add" };

    let dialog = gtk::Window::builder()
        .title(title)
        .transient_for(&state.window)
        .modal(true)
        .resizable(false)
        .default_width(360)
        .build();

    let prompt = gtk::Label::new(Some(&format!(
        "Marker at {}",
        format_marker_time(position_ns)
    )));
    prompt.set_xalign(0.0);
    let entry = gtk::Entry::builder()
        .text(&initial_name)
        .activates_default(true)
        .hexpand(true)
        .build();
    entry.select_region(0, -1);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.set_margin_top(12);
    form.set_margin_bottom(12);
    form.set_margin_start(12);
    form.set_margin_end(12);
    form.append(&prompt);
    form.append(&entry);

    let cancel_button = gtk::Button::with_label("Cancel");
    let add_button = gtk::Button::with_label(action_label);
    add_button.add_css_class("suggested-action");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.append(&cancel_button);
    actions.append(&add_button);
    form.append(&actions);
    dialog.set_child(Some(&form));
    dialog.set_default_widget(Some(&add_button));

    let weak_dialog = dialog.downgrade();
    cancel_button.connect_clicked(move |_| {
        if let Some(dialog) = weak_dialog.upgrade() {
            dialog.close();
        }
    });

    let weak = Rc::downgrade(state);
    let weak_dialog = dialog.downgrade();
    add_button.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            let entered_name = entry.text();
            let name = if entered_name.trim().is_empty() {
                initial_name.clone()
            } else {
                entered_name.trim().to_owned()
            };
            add_named_marker(&state, position_ns, name);
        }
        if let Some(dialog) = weak_dialog.upgrade() {
            dialog.close();
        }
    });
    dialog.present();
}

fn add_named_marker(state: &Rc<UiState>, position_ns: u64, name: String) {
    let mut markers = state.markers.borrow_mut();
    match markers.binary_search_by_key(&position_ns, |marker| marker.position_ns) {
        Ok(index) => markers[index].name = name,
        Err(index) => markers.insert(index, Marker { position_ns, name }),
    }
    drop(markers);

    marker_data_changed(state);
}

fn delete_marker(state: &Rc<UiState>, position_ns: u64) {
    let mut markers = state.markers.borrow_mut();
    if let Ok(index) = markers.binary_search_by_key(&position_ns, |marker| marker.position_ns) {
        markers.remove(index);
    }
    drop(markers);

    marker_data_changed(state);
}

fn marker_data_changed(state: &Rc<UiState>) {
    rebuild_marker_list(state);
    state.waveform.queue_draw();
    if let Err(error) = save_markers(state) {
        show_error(&state.window, "Could not save markers", &error.to_string());
    }
}

fn rebuild_marker_list(state: &Rc<UiState>) {
    while let Some(child) = state.marker_list.first_child() {
        state.marker_list.remove(&child);
    }

    for marker in state.markers.borrow().iter() {
        let position_ns = marker.position_ns;
        let marker_name = marker.name.clone();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let seek_button = gtk::Button::builder()
            .label(format!(
                "{} — {}",
                marker.name,
                format_marker_time(position_ns)
            ))
            .halign(gtk::Align::Fill)
            .hexpand(true)
            .focus_on_click(false)
            .tooltip_text("Seek to marker; right-click for options")
            .build();
        let weak = Rc::downgrade(state);
        seek_button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                seek_to_position(&state, gst::ClockTime::from_nseconds(position_ns));
            }
        });
        let rename_popover = gtk::Popover::new();
        let popover_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let context_rename_button = gtk::Button::builder()
            .label("Rename")
            .icon_name("document-edit-symbolic")
            .build();
        context_rename_button.add_css_class("flat");
        popover_content.append(&context_rename_button);
        rename_popover.set_child(Some(&popover_content));
        rename_popover.set_parent(&row);

        let weak = Rc::downgrade(state);
        let context_marker_name = marker_name.clone();
        let context_popover = rename_popover.clone();
        context_rename_button.connect_clicked(move |_| {
            context_popover.popdown();
            if let Some(state) = weak.upgrade() {
                show_marker_name_dialog(&state, position_ns, context_marker_name.clone(), true);
            }
        });

        let context_gesture = gtk::GestureClick::new();
        context_gesture.set_button(3);
        let context_popover = rename_popover.clone();
        context_gesture.connect_released(move |gesture, _press_count, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let pointing_to = gtk::gdk::Rectangle::new(x.round() as i32, y.round() as i32, 1, 1);
            context_popover.set_pointing_to(Some(&pointing_to));
            context_popover.popup();
        });
        row.add_controller(context_gesture);

        let rename_button = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename marker")
            .build();
        let weak = Rc::downgrade(state);
        rename_button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                show_marker_name_dialog(&state, position_ns, marker_name.clone(), true);
            }
        });
        let delete_button = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete marker")
            .build();
        let weak = Rc::downgrade(state);
        delete_button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                delete_marker(&state, position_ns);
            }
        });
        row.append(&seek_button);
        row.append(&rename_button);
        row.append(&delete_button);
        state.marker_list.append(&row);
    }
}

fn save_markers(state: &UiState) -> anyhow::Result<()> {
    let store = state.marker_store.borrow();
    let Some(store) = store.as_ref() else {
        return Ok(());
    };
    store.save(&state.markers.borrow())
}

fn save_loops(state: &UiState) -> anyhow::Result<()> {
    let store = state.loop_store.borrow();
    let Some(store) = store.as_ref() else {
        return Ok(());
    };
    store.save(&state.loops.borrow())
}

fn next_generic_loop_name(loops: &[LoopRegion]) -> String {
    let mut index = 1;
    loop {
        let name = format!("Loop {index}");
        if loops.iter().all(|region| region.name != name) {
            return name;
        }
        index += 1;
    }
}

fn add_loop(state: &Rc<UiState>, first_ns: u64, second_ns: u64) {
    let name = next_generic_loop_name(&state.loops.borrow());
    let Some(region) = LoopRegion::new(name, first_ns, second_ns) else {
        return;
    };
    let index = state.loops.borrow().len();
    state.loops.borrow_mut().push(region);
    state.active_loop.set(Some(index));
    loop_data_changed(state);
}

fn delete_loop(state: &Rc<UiState>, index: usize) {
    if index >= state.loops.borrow().len() {
        return;
    }
    state.loops.borrow_mut().remove(index);
    state.active_loop.set(match state.active_loop.get() {
        Some(active) if active == index => None,
        Some(active) if active > index => Some(active - 1),
        active => active,
    });
    loop_data_changed(state);
}

fn loop_data_changed(state: &Rc<UiState>) {
    rebuild_loop_list(state);
    state.waveform.queue_draw();
    if let Err(error) = save_loops(state) {
        show_error(&state.window, "Could not save loops", &error.to_string());
    }
}

fn rebuild_loop_list(state: &Rc<UiState>) {
    while let Some(child) = state.loop_list.first_child() {
        state.loop_list.remove(&child);
    }

    for (index, region) in state.loops.borrow().iter().enumerate() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let active = gtk::CheckButton::with_label(&format!(
            "{} — {}–{}",
            region.name,
            format_marker_time(region.start_ns),
            format_marker_time(region.end_ns)
        ));
        active.set_active(state.active_loop.get() == Some(index));
        active.set_hexpand(true);
        active.set_halign(gtk::Align::Fill);
        active.set_tooltip_text(Some("Enable this loop and seek to its A point"));
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete loop")
            .build();

        let weak = Rc::downgrade(state);
        active.connect_toggled(move |button| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if button.is_active() {
                state.active_loop.set(Some(index));
                let start_ns = state
                    .loops
                    .borrow()
                    .get(index)
                    .map(|region| region.start_ns);
                if let Some(start_ns) = start_ns {
                    seek_to_position(&state, gst::ClockTime::from_nseconds(start_ns));
                }
            } else if state.active_loop.get() == Some(index) {
                state.active_loop.set(None);
            }
            rebuild_loop_list(&state);
            state.waveform.queue_draw();
        });

        let weak = Rc::downgrade(state);
        delete.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                delete_loop(&state, index);
            }
        });

        row.append(&active);
        row.append(&delete);
        state.loop_list.append(&row);
    }
}

fn connect_keyboard_controls(state: &Rc<UiState>) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let weak = Rc::downgrade(state);
    controller.connect_key_pressed(move |_controller, key, _keycode, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        // Bindings are data-driven so settings changes take effect immediately.
        let command = state
            .key_bindings
            .borrow()
            .iter()
            .find(|binding| binding.matches(key, modifiers))
            .map(|binding| binding.command);
        let Some(command) = command else {
            return glib::Propagation::Proceed;
        };
        execute_command(&state, command);
        glib::Propagation::Stop
    });
    state.window.add_controller(controller);
}

fn execute_command(state: &Rc<UiState>, command: Command) {
    match command {
        Command::TogglePlayback => toggle_current_playback(state),
        Command::PlayPauseFromAnchor => play_from_anchor(state, true),
        Command::PlayFromAnchor => play_from_anchor(state, false),
        Command::Stop => state.stop_button.emit_clicked(),
        Command::GoToBeginning => seek_to_position(state, gst::ClockTime::ZERO),
        Command::GoToEnd => state.end_button.emit_clicked(),
        Command::SeekBackward1 => seek_relative(state, -1),
        Command::SeekForward1 => seek_relative(state, 1),
        Command::SeekBackward5 => seek_relative(state, -5),
        Command::SeekForward5 => seek_relative(state, 5),
        Command::SeekBackward10 => seek_relative(state, -10),
        Command::SeekForward10 => seek_relative(state, 10),
        Command::PreviousMarker => jump_to_marker(state, MarkerDirection::Previous),
        Command::NextMarker => jump_to_marker(state, MarkerDirection::Next),
        Command::AddMarker => state.add_marker_button.emit_clicked(),
    }
}

#[derive(Clone, Copy)]
enum MarkerDirection {
    Previous,
    Next,
}

fn jump_to_marker(state: &Rc<UiState>, direction: MarkerDirection) {
    if !state.player.is_loaded() {
        return;
    }

    let current_ns = navigation_position(
        state.playing.get(),
        state.player.position(),
        state.playback_anchor.get(),
    )
    .nseconds();
    let markers = state.markers.borrow();
    let Some(position_ns) = marker_jump_target(&markers, current_ns, direction) else {
        return;
    };
    drop(markers);

    seek_to_position(state, gst::ClockTime::from_nseconds(position_ns));
}

fn marker_jump_target(
    markers: &[Marker],
    current_ns: u64,
    direction: MarkerDirection,
) -> Option<u64> {
    match direction {
        MarkerDirection::Previous => markers.iter().rev().find(|marker| {
            marker.position_ns < current_ns.saturating_sub(MARKER_JUMP_TOLERANCE_NS)
        }),
        MarkerDirection::Next => markers.iter().find(|marker| {
            marker.position_ns > current_ns.saturating_add(MARKER_JUMP_TOLERANCE_NS)
        }),
    }
    .map(|marker| marker.position_ns)
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

    // A short drag is a seek click; a longer drag creates or adjusts a loop.
    let waveform_drag = gtk::GestureDrag::new();
    waveform_drag.set_button(1);
    let weak = Rc::downgrade(state);
    waveform_drag.connect_drag_begin(move |_gesture, x, _y| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let Some(duration) = state.duration.get().or_else(|| state.player.duration()) else {
            return;
        };
        let position_ns = waveform_x_to_ns(&state, x, duration.nseconds());
        let drag =
            active_loop_handle_at_x(&state, x, duration.nseconds()).unwrap_or(LoopDrag::New {
                start_ns: position_ns,
            });
        state.loop_drag.replace(Some(drag));
        if matches!(drag, LoopDrag::New { .. }) {
            state.loop_preview.set(Some((position_ns, position_ns)));
            state.waveform.queue_draw();
        }
    });

    let weak = Rc::downgrade(state);
    waveform_drag.connect_drag_update(move |gesture, offset_x, _offset_y| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let (Some(duration), Some((start_x, _))) = (
            state.duration.get().or_else(|| state.player.duration()),
            gesture.start_point(),
        ) else {
            return;
        };
        let position_ns = waveform_x_to_ns(&state, start_x + offset_x, duration.nseconds());
        let Some(drag) = *state.loop_drag.borrow() else {
            return;
        };
        match drag {
            LoopDrag::New { start_ns } => {
                state.loop_preview.set(Some((start_ns, position_ns)));
            }
            LoopDrag::Start { index } => {
                if let Some(region) = state.loops.borrow_mut().get_mut(index) {
                    region.start_ns = position_ns.min(region.end_ns.saturating_sub(1));
                }
            }
            LoopDrag::End { index } => {
                if let Some(region) = state.loops.borrow_mut().get_mut(index) {
                    region.end_ns = position_ns.max(region.start_ns.saturating_add(1));
                }
            }
        }
        state.waveform.queue_draw();
    });

    let weak = Rc::downgrade(state);
    waveform_drag.connect_drag_end(move |gesture, offset_x, _offset_y| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let drag = state.loop_drag.borrow_mut().take();
        let preview = state.loop_preview.take();
        let Some(duration) = state.duration.get().or_else(|| state.player.duration()) else {
            return;
        };
        match drag {
            Some(LoopDrag::New { start_ns }) if offset_x.abs() >= LOOP_DRAG_THRESHOLD_PX => {
                let end_ns = preview.map(|(_, end_ns)| end_ns).unwrap_or(start_ns);
                add_loop(&state, start_ns, end_ns);
            }
            Some(LoopDrag::New { .. }) => {
                let x = gesture.start_point().map(|(x, _)| x).unwrap_or(0.0);
                seek_to_position(
                    &state,
                    gst::ClockTime::from_nseconds(waveform_x_to_ns(&state, x, duration.nseconds())),
                );
            }
            Some(LoopDrag::Start { .. } | LoopDrag::End { .. }) => {
                loop_data_changed(&state);
            }
            None => {}
        }
        state.waveform.queue_draw();
    });
    state.waveform.add_controller(waveform_drag);
}

fn active_loop_handle_at_x(state: &UiState, x: f64, duration_ns: u64) -> Option<LoopDrag> {
    let index = state.active_loop.get()?;
    let loops = state.loops.borrow();
    let region = loops.get(index)?;
    let width = state.waveform.width().max(1) as f64;
    let visible_start = state.waveform_adjustment.value();
    let visible_span = state.waveform_adjustment.page_size();
    let endpoint_x = |position_ns: u64| {
        (position_ns as f64 / duration_ns as f64 - visible_start) / visible_span * width
    };
    // Hit-test in screen pixels so handles remain usable at every zoom level.
    let start_distance = (endpoint_x(region.start_ns) - x).abs();
    let end_distance = (endpoint_x(region.end_ns) - x).abs();
    if start_distance.min(end_distance) > LOOP_HANDLE_HIT_RADIUS_PX {
        None
    } else if start_distance <= end_distance {
        Some(LoopDrag::Start { index })
    } else {
        Some(LoopDrag::End { index })
    }
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
            state.beginning_button.set_sensitive(true);
            state.stop_button.set_sensitive(true);
            state.play_button.set_sensitive(true);
            state.end_button.set_sensitive(true);
            state.add_marker_button.set_sensitive(true);
            state.duration.set(None);
            state.peaks.borrow_mut().clear();
            state.progress.set(0.0);
            set_playback_anchor(state, gst::ClockTime::ZERO, None);
            state.waveform_adjustment.set_value(0.0);

            let marker_store = MarkerStore::for_audio(path);
            match marker_store.load() {
                Ok(markers) => {
                    state.markers.replace(markers);
                }
                Err(error) => {
                    state.markers.borrow_mut().clear();
                    show_error(&state.window, "Could not load markers", &error.to_string());
                }
            };
            state.marker_store.replace(Some(marker_store));
            rebuild_marker_list(state);

            let loop_store = LoopStore::for_audio(path);
            match loop_store.load() {
                Ok(loops) => {
                    state.loops.replace(loops);
                }
                Err(error) => {
                    state.loops.borrow_mut().clear();
                    show_error(&state.window, "Could not load loops", &error.to_string());
                }
            }
            state.loop_store.replace(Some(loop_store));
            state.active_loop.set(None);
            state.loop_preview.set(None);
            state.loop_drag.borrow_mut().take();
            rebuild_loop_list(state);
            state.waveform.queue_draw();
            update_position(state, gst::ClockTime::ZERO, None);

            let uri = gio::File::for_path(path).uri().to_string();
            record_recent_file(state, path);
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
            if let Some((start_ns, _end_ns)) = active_loop_bounds(state)
                && let Err(error) = state
                    .player
                    .seek(gst::ClockTime::from_nseconds(start_ns))
                    .and_then(|()| state.player.play())
            {
                show_error(&state.window, "Could not repeat loop", &error.to_string());
            } else if active_loop_bounds(state).is_some() {
                set_playing(state, true);
                return;
            }
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

fn active_loop_bounds(state: &UiState) -> Option<(u64, u64)> {
    let index = state.active_loop.get()?;
    state
        .loops
        .borrow()
        .get(index)
        .map(|region| (region.start_ns, region.end_ns))
}

fn repeat_active_loop_if_needed(state: &Rc<UiState>, position: gst::ClockTime) -> bool {
    let Some((start_ns, end_ns)) = active_loop_bounds(state) else {
        return false;
    };
    let Some(start_ns) = loop_repeat_target(start_ns, end_ns, position.nseconds()) else {
        return false;
    };
    // Player positions are source-time values, so the same bounds work at any speed.
    let start = gst::ClockTime::from_nseconds(start_ns);
    if let Err(error) = state.player.seek(start) {
        show_error(&state.window, "Could not repeat loop", &error.to_string());
        state.active_loop.set(None);
        rebuild_loop_list(state);
        return false;
    }
    update_position(state, start, state.player.duration());
    true
}

fn loop_repeat_target(start_ns: u64, end_ns: u64, position_ns: u64) -> Option<u64> {
    (start_ns < end_ns && position_ns >= end_ns).then_some(start_ns)
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

fn set_playing(state: &Rc<UiState>, playing: bool) {
    state.playing.set(playing);
    if playing {
        state
            .play_button
            .set_icon_name("media-playback-pause-symbolic");
        state
            .play_button
            .set_tooltip_text(Some("Pause at current position"));

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

                    if let Some(position) = state.player.position()
                        && !repeat_active_loop_if_needed(&state, position)
                    {
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
            .set_tooltip_text(Some("Play at current position"));
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

#[cfg(test)]
mod tests {
    use super::{
        Marker, MarkerDirection, escape_menu_label, loop_repeat_target, marker_jump_target,
        next_generic_marker_name,
    };

    #[test]
    fn marker_jumps_move_to_the_adjacent_marker() {
        let markers = [1, 2, 3].map(|seconds| Marker {
            position_ns: seconds * 1_000_000_000,
            name: seconds.to_string(),
        });

        assert_eq!(
            marker_jump_target(&markers, 2_000_000_000, MarkerDirection::Previous),
            Some(1_000_000_000)
        );
        assert_eq!(
            marker_jump_target(&markers, 2_000_000_000, MarkerDirection::Next),
            Some(3_000_000_000)
        );
    }

    #[test]
    fn loop_repeats_at_or_beyond_its_b_point() {
        assert_eq!(loop_repeat_target(10, 20, 19), None);
        assert_eq!(loop_repeat_target(10, 20, 20), Some(10));
        assert_eq!(loop_repeat_target(10, 20, 25), Some(10));
        assert_eq!(loop_repeat_target(20, 20, 20), None);
    }

    #[test]
    fn generic_marker_names_fill_the_first_available_number() {
        let markers = ["Marker 1", "Marker 3"].map(|name| Marker {
            position_ns: 0,
            name: name.into(),
        });
        assert_eq!(next_generic_marker_name(&markers), "Marker 2");
    }

    #[test]
    fn recent_menu_labels_preserve_underscores() {
        assert_eq!(
            escape_menu_label("first_interview.flac"),
            "first__interview.flac"
        );
    }
}
