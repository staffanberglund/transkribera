use std::{cell::RefCell, rc::Rc};

use gstreamer as gst;
use gtk::{glib, prelude::*};

use crate::{
    preferences::Preferences,
    shortcuts::{Command, KeyBinding, accelerator_for_event, default_key_bindings},
};

use super::{
    UiState,
    marker_ui::{MarkerDirection, jump_to_marker},
    navigation::{seek_relative, seek_to_position},
    playback_ui::{play_from_anchor, toggle_current_playback},
    show_error,
};

pub(super) fn connect_settings_button(state: &Rc<UiState>, settings_button: &gtk::Button) {
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

pub(super) fn connect_keyboard_controls(state: &Rc<UiState>) {
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
