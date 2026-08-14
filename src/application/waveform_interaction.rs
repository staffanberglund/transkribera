use std::{cell::Cell, rc::Rc};

use gtk::{glib, prelude::*};

use super::{MIN_WAVEFORM_ZOOM, UiState};
use crate::waveform::render::MAX_WAVEFORM_ZOOM;

const ZOOM_OCTAVE_SCROLL_UNITS: f64 = 4.0;
const PAN_FRACTION_PER_SCROLL_UNIT: f64 = 0.1;

pub(super) fn connect_waveform_zoom(state: &Rc<UiState>) {
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

pub(super) fn connect_waveform_scroll_zoom(state: &Rc<UiState>) {
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

pub(super) fn connect_waveform_pinch_zoom(state: &Rc<UiState>) {
    let gesture = gtk::GestureZoom::new();
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let starting_zoom = Rc::new(Cell::new(MIN_WAVEFORM_ZOOM));
    let focus = Rc::new(Cell::new((0.5, 0.5)));

    let weak = Rc::downgrade(state);
    let begin_zoom = Rc::clone(&starting_zoom);
    let begin_focus = Rc::clone(&focus);
    gesture.connect_begin(move |gesture, _sequence| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        // GTK reports cumulative scale from gesture start, so retain the initial zoom.
        begin_zoom.set(state.waveform_zoom.value());
        let width = state.waveform.width().max(1) as f64;
        let focus_x = gesture
            .bounding_box_center()
            .map(|(x, _)| x / width)
            .unwrap_or_else(|| state.waveform_pointer.get())
            .clamp(0.0, 1.0);
        let focus_time = timeline_fraction_at_x(
            state.waveform_adjustment.value(),
            state.waveform_adjustment.page_size(),
            focus_x,
            1.0,
        );
        begin_focus.set((focus_time.clamp(0.0, 1.0), focus_x));
    });

    let weak = Rc::downgrade(state);
    gesture.connect_scale_changed(move |gesture, scale| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let current = state.waveform_zoom.value();
        let next = (starting_zoom.get() * scale).clamp(MIN_WAVEFORM_ZOOM, MAX_WAVEFORM_ZOOM);
        if (next - current).abs() <= f64::EPSILON {
            return;
        }
        state.pending_zoom_focus.set(Some(focus.get()));
        state.waveform_zoom.set_value(next);
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    state.waveform.add_controller(gesture);
}

pub(super) fn waveform_x_to_ns(state: &UiState, x: f64, duration_ns: u64) -> u64 {
    let fraction = timeline_fraction_at_x(
        state.waveform_adjustment.value(),
        state.waveform_adjustment.page_size(),
        x,
        state.waveform.width().max(1) as f64,
    );
    (duration_ns as f64 * fraction).round() as u64
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

pub(super) fn keep_playback_cursor_visible(state: &UiState) {
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
