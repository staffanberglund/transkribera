use std::rc::Rc;

use gstreamer as gst;
use gtk::prelude::*;

use super::{UiState, show_error, waveform_interaction::keep_playback_cursor_visible};

pub(super) fn navigation_position(
    playing: bool,
    player_position: Option<gst::ClockTime>,
    playback_anchor: gst::ClockTime,
) -> gst::ClockTime {
    if playing {
        player_position.unwrap_or(playback_anchor)
    } else {
        playback_anchor
    }
}

pub(super) fn seek_relative(state: &Rc<UiState>, seconds: i64) {
    if !state.player.is_loaded() {
        return;
    }

    let current = state
        .player
        .position()
        .unwrap_or_else(|| state.playback_anchor.get());
    let current_ns = i128::from(current.nseconds());
    let offset_ns = i128::from(seconds) * 1_000_000_000;
    let duration_ns = state
        .duration
        .get()
        .or_else(|| state.player.duration())
        .map(|duration| i128::from(duration.nseconds()))
        .unwrap_or(i128::MAX);
    let target_ns = (current_ns + offset_ns).clamp(0, duration_ns) as u64;
    seek_to_position(state, gst::ClockTime::from_nseconds(target_ns));
}

pub(super) fn seek_to_position(state: &Rc<UiState>, position: gst::ClockTime) {
    if !state.player.is_loaded() {
        return;
    }

    let duration = state.duration.get().or_else(|| state.player.duration());
    let position = duration
        .map(|duration| position.min(duration))
        .unwrap_or(position);
    if let Err(error) = state.player.seek(position) {
        show_error(&state.window, "Could not seek", &error.to_string());
    } else {
        set_playback_anchor(state, position, duration);
        update_position(state, position, duration);
    }
}

pub(super) fn update_position(
    state: &UiState,
    position: gst::ClockTime,
    duration: Option<gst::ClockTime>,
) {
    if let Some(duration) = duration {
        state.duration.set(Some(duration));
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

pub(super) fn set_playback_anchor(
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

pub(super) fn update_time_label(
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

pub(super) fn format_marker_time(position_ns: u64) -> String {
    let total_milliseconds = position_ns / 1_000_000;
    let milliseconds = total_milliseconds % 1_000;
    let total_seconds = total_milliseconds / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
    } else {
        format!("{minutes:02}:{seconds:02}.{milliseconds:03}")
    }
}

#[cfg(test)]
mod tests {
    use gstreamer as gst;

    use super::navigation_position;

    #[test]
    fn paused_marker_navigation_uses_the_exact_playback_anchor() {
        let stale_pipeline_position = gst::ClockTime::from_seconds(1);
        let marker_anchor = gst::ClockTime::from_seconds(2);
        assert_eq!(
            navigation_position(false, Some(stale_pipeline_position), marker_anchor),
            marker_anchor
        );
        assert_eq!(
            navigation_position(true, Some(stale_pipeline_position), marker_anchor),
            stale_pipeline_position
        );
    }
}
