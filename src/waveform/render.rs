use gtk::cairo;

use crate::{loops::LoopRegion, markers::Marker};

pub(crate) const MAX_WAVEFORM_ZOOM: f64 = 100.0;

pub(crate) struct WaveformView<'a> {
    pub(crate) peaks: &'a [f32],
    pub(crate) markers: &'a [Marker],
    pub(crate) loops: &'a [LoopRegion],
    pub(crate) active_loop: Option<usize>,
    pub(crate) loop_preview: Option<(u64, u64)>,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) progress: f64,
    pub(crate) anchor_progress: f64,
    pub(crate) visible_start: f64,
    pub(crate) visible_span: f64,
}

pub(crate) fn draw_waveform(
    context: &cairo::Context,
    width: f64,
    height: f64,
    view: WaveformView<'_>,
) {
    // Keep the viewport within sensible zoom and pan limits.
    // Timeline positions are normalized: 0.0 is the start and 1.0 is the end.
    let visible_span = view.visible_span.clamp(1.0 / MAX_WAVEFORM_ZOOM, 1.0);
    let visible_start = view.visible_start.clamp(0.0, (1.0 - visible_span).max(0.0));
    let visible_end = visible_start + visible_span;

    // Paint the whole drawing area before adding timeline layers.
    context.set_source_rgb(0.16, 0.16, 0.18);
    let _ = context.paint();

    // Center line for the waveform baseline.
    let center = height / 2.0;
    context.set_source_rgba(1.0, 1.0, 1.0, 0.20);
    context.set_line_width(1.0);
    context.move_to(0.0, center.floor() + 0.5);
    context.line_to(width, center.floor() + 0.5);
    let _ = context.stroke();

    // Draw loop regions before the peaks so they sit behind the waveform.
    if let Some(duration_ns) = view.duration_ns.filter(|duration| *duration > 0) {
        for (index, region) in view.loops.iter().enumerate() {
            draw_loop_region(
                context,
                width,
                height,
                duration_ns,
                visible_start,
                visible_span,
                region.start_ns,
                region.end_ns,
                view.active_loop == Some(index),
            );
        }
        // While dragging, preview the unsaved region using the active-loop style.
        if let Some((first_ns, second_ns)) = view.loop_preview {
            draw_loop_region(
                context,
                width,
                height,
                duration_ns,
                visible_start,
                visible_span,
                first_ns.min(second_ns),
                first_ns.max(second_ns),
                true,
            );
        }
    }

    if !view.peaks.is_empty() {
        // First draw the complete waveform in its neutral color.
        draw_peaks(
            context,
            width,
            height,
            view.peaks,
            visible_start,
            visible_span,
            (0.68, 0.68, 0.72),
        );

        // Redraw only the elapsed portion in blue by clipping at the playhead.
        // Saving the Cairo state keeps this clip from affecting later layers.
        context.save().ok();
        let cursor_x = ((view.progress - visible_start) / visible_span * width).clamp(0.0, width);
        context.rectangle(0.0, 0.0, cursor_x, height);
        context.clip();
        draw_peaks(
            context,
            width,
            height,
            view.peaks,
            visible_start,
            visible_span,
            (0.22, 0.62, 0.96),
        );
        context.restore().ok();
    }

    if let Some(duration_ns) = view.duration_ns.filter(|duration| *duration > 0) {
        // Convert absolute marker times to normalized timeline positions.
        context.set_source_rgba(0.38, 0.85, 0.55, 0.9);
        context.set_line_width(1.0);
        for marker in view.markers {
            let marker_progress = marker.position_ns as f64 / duration_ns as f64;
            if (visible_start..=visible_end).contains(&marker_progress) {
                // Subtract the viewport start, then scale its span to screen pixels.
                let marker_x = (marker_progress - visible_start) / visible_span * width;
                // Half-pixel alignment keeps a one-pixel Cairo line crisp.
                context.move_to(marker_x.floor() + 0.5, 0.0);
                context.line_to(marker_x.floor() + 0.5, height);
            }
        }
        let _ = context.stroke();
    }

    // The orange anchor remembers where Space/P will restart playback.
    if (visible_start..=visible_end).contains(&view.anchor_progress) {
        let anchor_x = (view.anchor_progress - visible_start) / visible_span * width;
        context.set_source_rgb(1.0, 0.27, 0.18);
        context.set_line_width(2.5);
        context.move_to(anchor_x, 0.0);
        context.line_to(anchor_x, height);
        let _ = context.stroke();
    }

    // The white playhead follows the current live playback position.
    if (visible_start..=visible_end).contains(&view.progress) {
        let cursor_x = (view.progress - visible_start) / visible_span * width;
        context.set_source_rgb(0.95, 0.95, 0.98);
        context.set_line_width(1.5);
        context.move_to(cursor_x, 0.0);
        context.line_to(cursor_x, height);
        let _ = context.stroke();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_loop_region(
    context: &cairo::Context,
    width: f64,
    height: f64,
    duration_ns: u64,
    visible_start: f64,
    visible_span: f64,
    start_ns: u64,
    end_ns: u64,
    active: bool,
) {
    if start_ns >= end_ns {
        return;
    }
    let start_fraction = start_ns as f64 / duration_ns as f64;
    let end_fraction = end_ns as f64 / duration_ns as f64;
    let start_x = (start_fraction - visible_start) / visible_span * width;
    let end_x = (end_fraction - visible_start) / visible_span * width;
    let left = start_x.clamp(0.0, width);
    let right = end_x.clamp(0.0, width);
    if right <= 0.0 || left >= width || right <= left {
        return;
    }
    if active {
        context.set_source_rgba(0.95, 0.45, 0.18, 0.22);
    } else {
        context.set_source_rgba(0.95, 0.45, 0.18, 0.10);
    }
    context.rectangle(left, 0.0, right - left, height);
    let _ = context.fill();

    context.set_source_rgba(1.0, 0.55, 0.20, if active { 0.95 } else { 0.55 });
    context.set_line_width(if active { 2.5 } else { 1.0 });
    for x in [start_x, end_x] {
        if (0.0..=width).contains(&x) {
            context.move_to(x, 0.0);
            context.line_to(x, height);
            let _ = context.stroke();
            if active {
                context.rectangle(x - 4.0, 0.0, 8.0, 12.0);
                let _ = context.fill();
            }
        }
    }
}

fn draw_peaks(
    context: &cairo::Context,
    width: f64,
    height: f64,
    peaks: &[f32],
    visible_start: f64,
    visible_span: f64,
    color: (f64, f64, f64),
) {
    if width <= 0.0 || peaks.is_empty() {
        return;
    }

    let center = height / 2.0;
    let amplitude = (center - 8.0).max(1.0);
    let (clip_start, _, clip_end, _) = context.clip_extents().unwrap_or((0.0, 0.0, width, height));
    let clip_start = clip_start.clamp(0.0, width);
    let clip_end = clip_end.clamp(0.0, width);
    if clip_end <= clip_start {
        return;
    }

    context.set_source_rgb(color.0, color.1, color.2);
    context.set_line_width(1.0);

    let visible_peak_count = peaks.len() as f64 * visible_span;
    if visible_peak_count >= width {
        let first_column = clip_start.floor().max(0.0) as usize;
        let last_column = clip_end.ceil().min(width.ceil()) as usize;

        for column in first_column..last_column {
            let start_fraction = visible_start + column as f64 / width * visible_span;
            let end_fraction = visible_start + (column + 1) as f64 / width * visible_span;
            let start = (start_fraction * peaks.len() as f64)
                .floor()
                .clamp(0.0, (peaks.len() - 1) as f64) as usize;
            let end = (end_fraction * peaks.len() as f64)
                .ceil()
                .max((start + 1) as f64)
                .min(peaks.len() as f64) as usize;
            let peak = peaks[start..end].iter().copied().fold(0.0_f32, f32::max) as f64;
            let x = column as f64 + 0.5;
            context.move_to(x, center - peak * amplitude);
            context.line_to(x, center + peak * amplitude);
        }
    } else {
        let clip_time_start = visible_start + clip_start / width * visible_span;
        let clip_time_end = visible_start + clip_end / width * visible_span;
        let first_peak = (clip_time_start * peaks.len() as f64).floor().max(0.0) as usize;
        let first_peak = first_peak.saturating_sub(1);
        let last_peak = ((clip_time_end * peaks.len() as f64).ceil() as usize + 1).min(peaks.len());

        for (index, peak) in peaks.iter().enumerate().take(last_peak).skip(first_peak) {
            let peak_time = (index as f64 + 0.5) / peaks.len() as f64;
            let x = (peak_time - visible_start) / visible_span * width;
            let peak = f64::from(*peak);
            context.move_to(x, center - peak * amplitude);
            context.line_to(x, center + peak * amplitude);
        }
    }
    let _ = context.stroke();
}
