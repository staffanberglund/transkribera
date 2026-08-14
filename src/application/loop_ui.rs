use std::rc::Rc;

use gstreamer as gst;
use gtk::prelude::*;

use super::{
    LoopDrag, UiState,
    navigation::{format_marker_time, seek_to_position, update_position},
    show_error,
    waveform_interaction::waveform_x_to_ns,
};
use crate::loops::LoopRegion;

const LOOP_HANDLE_HIT_RADIUS_PX: f64 = 8.0;
const LOOP_DRAG_THRESHOLD_PX: f64 = 3.0;

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

pub(super) fn rebuild_loop_list(state: &Rc<UiState>) {
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

pub(super) fn connect_waveform_loop_gesture(state: &Rc<UiState>) {
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

pub(super) fn active_loop_bounds(state: &UiState) -> Option<(u64, u64)> {
    let index = state.active_loop.get()?;
    state
        .loops
        .borrow()
        .get(index)
        .map(|region| (region.start_ns, region.end_ns))
}

pub(super) fn repeat_active_loop_if_needed(state: &Rc<UiState>, position: gst::ClockTime) -> bool {
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

#[cfg(test)]
mod tests {
    use super::loop_repeat_target;

    #[test]
    fn loop_repeats_at_or_beyond_its_b_point() {
        assert_eq!(loop_repeat_target(10, 20, 19), None);
        assert_eq!(loop_repeat_target(10, 20, 20), Some(10));
        assert_eq!(loop_repeat_target(10, 20, 25), Some(10));
        assert_eq!(loop_repeat_target(20, 20, 20), None);
    }
}
