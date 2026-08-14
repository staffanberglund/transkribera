use std::rc::Rc;

use gstreamer as gst;
use gtk::prelude::*;

use super::{
    UiState,
    navigation::{format_marker_time, navigation_position, seek_to_position},
    show_error,
};
use crate::markers::Marker;

const MARKER_JUMP_TOLERANCE_NS: u64 = 50_000_000;

pub(super) fn connect_marker_controls(state: &Rc<UiState>) {
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

pub(super) fn rebuild_marker_list(state: &Rc<UiState>) {
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

#[derive(Clone, Copy)]
pub(super) enum MarkerDirection {
    Previous,
    Next,
}

pub(super) fn jump_to_marker(state: &Rc<UiState>, direction: MarkerDirection) {
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

#[cfg(test)]
mod tests {
    use super::{Marker, MarkerDirection, marker_jump_target, next_generic_marker_name};

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
    fn generic_marker_names_fill_the_first_available_number() {
        let markers = ["Marker 1", "Marker 3"].map(|name| Marker {
            position_ns: 0,
            name: name.into(),
        });
        assert_eq!(next_generic_marker_name(&markers), "Marker 2");
    }
}
