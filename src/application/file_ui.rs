use std::{path::Path, rc::Rc};

use gstreamer as gst;
use gtk::{gio, prelude::*};

use crate::{
    loops::LoopStore, markers::MarkerStore, recent::record as record_recent, waveform::WaveformJob,
};

use super::{
    UiState,
    loop_ui::rebuild_loop_list,
    marker_ui::rebuild_marker_list,
    navigation::{set_playback_anchor, update_position},
    playback_ui::set_playing,
    show_error,
};

pub(super) fn connect_open_button(state: &Rc<UiState>, open_button: &gtk::Button) {
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

pub(super) fn rebuild_recent_menu(state: &Rc<UiState>) {
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

pub(super) fn connect_recent_action(state: &Rc<UiState>) {
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

#[cfg(test)]
mod tests {
    use super::escape_menu_label;

    #[test]
    fn recent_menu_labels_preserve_underscores() {
        assert_eq!(
            escape_menu_label("first_interview.flac"),
            "first__interview.flac"
        );
    }
}
