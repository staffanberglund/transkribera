use std::rc::Rc;

use gstreamer as gst;
use gtk::{glib, prelude::*};

use crate::player::PlayerEvent;

use super::{
    UiState,
    loop_ui::{active_loop_bounds, repeat_active_loop_if_needed},
    navigation::{seek_to_position, set_playback_anchor, update_position, update_time_label},
    poll_waveform_job, show_error,
};

const UPDATE_INTERVAL_MS: u64 = 150;

pub(super) fn connect_playback_controls(state: &Rc<UiState>) {
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

pub(super) fn connect_volume_control(state: &Rc<UiState>, volume: &gtk::ScaleButton) {
    let weak = Rc::downgrade(state);
    volume.connect_value_changed(move |_button, value| {
        if let Some(state) = weak.upgrade() {
            state.player.set_volume(value);
        }
    });
}

pub(super) fn connect_speed_control(
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

pub(super) fn toggle_current_playback(state: &Rc<UiState>) {
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

pub(super) fn play_from_anchor(state: &Rc<UiState>, toggle_pause: bool) {
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

pub(super) fn connect_seek_scale(state: &Rc<UiState>) {
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
}

pub(super) fn start_ui_timer(state: &Rc<UiState>) -> glib::SourceId {
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

pub(super) fn set_playing(state: &Rc<UiState>, playing: bool) {
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
