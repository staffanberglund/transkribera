use gtk::gdk;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    TogglePlayback,
    PlayPauseFromAnchor,
    PlayFromAnchor,
    Stop,
    GoToBeginning,
    GoToEnd,
    SeekBackward1,
    SeekForward1,
    SeekBackward5,
    SeekForward5,
    SeekBackward10,
    SeekForward10,
    PreviousMarker,
    NextMarker,
    AddMarker,
}

impl Command {
    pub const ALL: [Self; 15] = [
        Self::TogglePlayback,
        Self::PlayPauseFromAnchor,
        Self::PlayFromAnchor,
        Self::Stop,
        Self::GoToBeginning,
        Self::GoToEnd,
        Self::SeekBackward1,
        Self::SeekForward1,
        Self::SeekBackward5,
        Self::SeekForward5,
        Self::SeekBackward10,
        Self::SeekForward10,
        Self::PreviousMarker,
        Self::NextMarker,
        Self::AddMarker,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::TogglePlayback => "toggle_playback",
            Self::PlayPauseFromAnchor => "play_pause_from_anchor",
            Self::PlayFromAnchor => "play_from_anchor",
            Self::Stop => "stop",
            Self::GoToBeginning => "go_to_beginning",
            Self::GoToEnd => "go_to_end",
            Self::SeekBackward1 => "seek_backward_1",
            Self::SeekForward1 => "seek_forward_1",
            Self::SeekBackward5 => "seek_backward_5",
            Self::SeekForward5 => "seek_forward_5",
            Self::SeekBackward10 => "seek_backward_10",
            Self::SeekForward10 => "seek_forward_10",
            Self::PreviousMarker => "previous_marker",
            Self::NextMarker => "next_marker",
            Self::AddMarker => "add_marker",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TogglePlayback => "Play/pause at current position",
            Self::PlayPauseFromAnchor => "Play/pause from playhead",
            Self::PlayFromAnchor => "Play from playhead",
            Self::Stop => "Stop",
            Self::GoToBeginning => "Go to beginning",
            Self::GoToEnd => "Go to end",
            Self::SeekBackward1 => "Seek backward 1 second",
            Self::SeekForward1 => "Seek forward 1 second",
            Self::SeekBackward5 => "Seek backward 5 seconds",
            Self::SeekForward5 => "Seek forward 5 seconds",
            Self::SeekBackward10 => "Seek backward 10 seconds",
            Self::SeekForward10 => "Seek forward 10 seconds",
            Self::PreviousMarker => "Previous marker",
            Self::NextMarker => "Next marker",
            Self::AddMarker => "Add marker",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    pub command: Command,
    pub accelerator: String,
}

impl KeyBinding {
    pub fn new(command: Command, accelerator: impl Into<String>) -> Self {
        Self {
            command,
            accelerator: accelerator.into(),
        }
    }

    pub fn matches(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        let Some((binding_key, binding_modifiers)) = gtk::accelerator_parse(&self.accelerator)
        else {
            return false;
        };
        let mask = gtk::accelerator_get_default_mod_mask();
        binding_key == key.to_lower() && binding_modifiers == modifiers & mask
    }

    pub fn display_label(&self) -> String {
        gtk::accelerator_parse(&self.accelerator)
            .map(|(key, modifiers)| gtk::accelerator_get_label(key, modifiers).to_string())
            .unwrap_or_else(|| self.accelerator.clone())
    }
}

pub fn default_key_bindings() -> Vec<KeyBinding> {
    [
        (Command::TogglePlayback, "k"),
        (Command::PlayPauseFromAnchor, "space"),
        (Command::PlayFromAnchor, "p"),
        (Command::SeekBackward10, "j"),
        (Command::SeekForward10, "l"),
        (Command::SeekBackward1, "Left"),
        (Command::SeekForward1, "Right"),
        (Command::SeekBackward5, "<Shift>Left"),
        (Command::SeekForward5, "<Shift>Right"),
        (Command::PreviousMarker, "<Alt>Left"),
        (Command::NextMarker, "<Alt>Right"),
        (Command::PreviousMarker, "Page_Up"),
        (Command::NextMarker, "Page_Down"),
    ]
    .into_iter()
    .map(|(command, accelerator)| KeyBinding::new(command, accelerator))
    .collect()
}

pub fn accelerator_for_event(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<String> {
    if matches!(
        key,
        gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Meta_L
            | gdk::Key::Meta_R
    ) {
        return None;
    }
    let modifiers = modifiers & gtk::accelerator_get_default_mod_mask();
    Some(gtk::accelerator_name(key.to_lower(), modifiers).to_string())
}

#[cfg(test)]
mod tests {
    use super::{Command, default_key_bindings};

    #[test]
    fn command_ids_are_unique_and_round_trip() {
        for (index, command) in Command::ALL.iter().enumerate() {
            assert_eq!(Command::from_id(command.id()), Some(*command));
            assert!(
                Command::ALL[..index]
                    .iter()
                    .all(|earlier| earlier.id() != command.id())
            );
        }
    }

    #[test]
    fn defaults_include_five_second_seeks_and_both_marker_key_pairs() {
        let bindings = default_key_bindings();
        for (command, accelerator) in [
            (Command::SeekBackward5, "<Shift>Left"),
            (Command::SeekForward5, "<Shift>Right"),
            (Command::PreviousMarker, "<Alt>Left"),
            (Command::NextMarker, "<Alt>Right"),
            (Command::PreviousMarker, "Page_Up"),
            (Command::NextMarker, "Page_Down"),
        ] {
            assert!(bindings.iter().any(|binding| {
                binding.command == command && binding.accelerator == accelerator
            }));
        }
    }
}
