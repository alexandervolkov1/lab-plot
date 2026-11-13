//! Presentation uses original samples, never interpolated values across gaps.
use crate::{
    plots::{PlotSample, PlotState},
    tcp_client::{Reading, ScalarValue},
};
use chrono::{DateTime, Local, Utc};
use eframe::egui;
use egui_plot::{GridInput, GridMark, ItemId, ItemIdSet, PlotTransform};
use std::ops::RangeInclusive;

fn local_time(seconds: f64) -> Option<DateTime<Local>> {
    if !(0.0..253402214400.0).contains(&seconds) {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis((seconds * 1000.0).round() as i64)
        .map(|t| t.with_timezone(&Local))
}

pub fn measurement_time(ms: u64) -> Option<String> {
    // Bound before conversion and local timezone arithmetic (also on Windows).
    if ms >= 253402214400000 {
        return None;
    }
    let time = DateTime::<Utc>::from_timestamp_millis(i64::try_from(ms).ok()?)?;
    Some(
        time.with_timezone(&Local)
            .format("%d.%m.%Y %H:%M:%S%.3f %:z")
            .to_string(),
    )
}

pub fn time_grid_steps(input: GridInput) -> [f64; 3] {
    // egui_plot's default minimum grid spacing is 8 points. Aim for time ticks
    // about 160 points apart, including in narrow windows and while zooming.
    let minimum = (input.base_step_size * (160.0 / 8.0)).max(0.001);
    let step = [
        0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0,
        60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0, 21600.0, 43200.0,
        86400.0,
    ]
    .into_iter()
    .find(|&step| step >= minimum)
    .unwrap_or_else(|| 86400.0 * (minimum / 86400.0).ceil());
    [step, step * 5.0, step * 10.0]
}

pub fn format_x_mark(mark: GridMark, range: &RangeInclusive<f64>, width: f32) -> String {
    let Some(time) = local_time(mark.value) else {
        return String::new();
    };
    let across_days = local_time(*range.start())
        .zip(local_time(*range.end()))
        .is_none_or(|(a, b)| a.date_naive() != b.date_naive());
    let format = if mark.step_size < 1.0 || time.timestamp_subsec_millis() != 0 {
        "%H:%M:%S%.3f"
    } else {
        "%H:%M:%S"
    };
    let mut label = time.format(format).to_string();
    if across_days {
        label = format!("{}\n{label}", time.format("%d.%m.%Y"));
    }
    let longest_line = label.lines().map(str::len).max().unwrap_or(0);
    let margin = (range.end() - range.start()).abs() * (longest_line as f64 * 3.5 + 8.0)
        / f64::from(width.max(1.0));
    if mark.value < range.start() + margin || mark.value > range.end() - margin {
        String::new()
    } else {
        label
    }
}

pub fn scalar_text(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Integer(v) => v.to_string(),
        ScalarValue::Float(v) => v.to_string(),
        ScalarValue::Boolean(v) => v.to_string(),
    }
}

pub fn sample_text(name: &str, unit: &str, sample: &PlotSample) -> Option<String> {
    if !sample.xy.iter().all(|v| v.is_finite()) {
        return None;
    }
    if let Some(m) = &sample.measurement {
        match &m.reading {
            Reading::Value { value, unit } if !matches!(value, ScalarValue::Boolean(_)) => {
                let time = measurement_time(m.completed_at_unix_ms)?;
                Some(format!("{name}\n{time}\n{} {unit}", scalar_text(value)))
            }
            _ => None,
        }
    } else {
        Some(format!(
            "{name}\nx = {}\n{} {unit}",
            sample.xy[0], sample.xy[1]
        ))
    }
}

/// A shared axis is temporal only when all retained points are observations.
/// Manual or mixed plots keep their numeric axis.
pub fn time_axis(plots: &PlotState) -> bool {
    let mut samples = plots.series.values().flat_map(|s| &s.points).peekable();
    samples.peek().is_some() && samples.all(|p| p.measurement.is_some())
}

pub fn nearest_sample<'a>(
    plots: &'a PlotState,
    transform: &PlotTransform,
    pointer: egui::Pos2,
    hidden: &ItemIdSet,
    radius: f32,
) -> Option<(&'a str, &'a str, &'a PlotSample)> {
    let mut nearest = None;
    let mut distance = radius * radius;
    for (name, series) in &plots.series {
        for range in series.segments() {
            if hidden.contains(&ItemId::new((name, series.sample_unit(range.start)))) {
                continue;
            }
            for index in range {
                let sample = &series.points[index];
                if !sample.xy.iter().all(|v| v.is_finite()) {
                    continue;
                }
                let screen = transform.position_from_point(&sample.xy.into());
                if !transform.frame().contains(screen) {
                    continue;
                }
                let d = screen.distance_sq(pointer);
                if d <= distance && sample_text(name, &series.unit, sample).is_some() {
                    distance = d;
                    nearest = Some((name.as_str(), series.unit.as_str(), sample));
                }
            }
        }
    }
    nearest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plots::PlotCommand, tcp_client::Measurement};
    use std::sync::Arc;
    #[test]
    fn time_labels_validate_range_and_show_date_and_subseconds() {
        for bad in [f64::NAN, f64::INFINITY, f64::MAX, -1.0] {
            assert!(local_time(bad).is_none());
        }
        assert!(measurement_time(u64::MAX).is_none());
        assert!(measurement_time(1794565335123).unwrap().contains(".123"));
        let mark = GridMark {
            value: 1794565335.0,
            step_size: 1.0,
        };
        assert!(format_x_mark(mark, &(1794500000.0..=1794800000.0), 800.0).contains("2026"));
        assert!(format_x_mark(mark, &(mark.value..=mark.value + 100.0), 520.0).is_empty());
        assert_eq!(
            format_x_mark(mark, &(mark.value - 100.0..=mark.value + 100.0), 800.0).len(),
            8
        );
        let fractional = GridMark {
            value: mark.value + 0.5,
            step_size: 2.5,
        };
        assert!(
            format_x_mark(fractional, &(mark.value - 10.0..=mark.value + 10.0), 800.0)
                .ends_with(".500")
        );
        let dated = format_x_mark(mark, &(mark.value - 86400.0..=mark.value + 86400.0), 800.0);
        assert!(
            dated.ends_with(
                &local_time(mark.value)
                    .unwrap()
                    .format("%H:%M:%S")
                    .to_string()
            )
        );
        assert_eq!(dated.lines().count(), 2);
    }
    #[test]
    fn tooltip_preserves_integer_and_rejects_invalid_and_boolean_samples() {
        let mut sample = PlotSample {
            xy: [1.0, 2.0],
            break_before: true,
            measurement: None,
        };
        for value in [ScalarValue::Integer(i64::MAX), ScalarValue::Float(68.4)] {
            sample.measurement = Some(Arc::new(Measurement {
                key: "a".into(),
                completed_at_unix_ms: 1234,
                reading: Reading::Value {
                    value,
                    unit: "degC".into(),
                },
            }));
            let text = sample_text("Heater", "", &sample).unwrap();
            assert!(text.contains(&scalar_text(&value)) && text.contains("degC"));
        }
        Arc::make_mut(sample.measurement.as_mut().unwrap()).reading = Reading::Value {
            value: ScalarValue::Boolean(true),
            unit: String::new(),
        };
        assert!(sample_text("a", "", &sample).is_none());
        sample.measurement = None;
        sample.xy[1] = f64::NAN;
        assert!(sample_text("a", "", &sample).is_none());
    }
    #[test]
    fn nearest_is_by_distance_ignores_hidden_curves_and_does_not_bridge_gaps() {
        let mut plots = PlotState::default();
        let transform = PlotTransform::new(
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0)),
            egui_plot::PlotBounds::from_min_max([0.0, 0.0], [100.0, 100.0]),
            false,
        );
        let mut hidden = ItemIdSet::default();
        assert!(
            nearest_sample(&plots, &transform, egui::pos2(50.0, 50.0), &hidden, 10.0).is_none()
        );
        for (name, x) in [("a", 50.0), ("z", 55.0)] {
            plots
                .apply(PlotCommand::Point {
                    name: name.into(),
                    time: x,
                    value: 50.0,
                })
                .unwrap();
        }
        assert_eq!(
            nearest_sample(&plots, &transform, egui::pos2(50.0, 50.0), &hidden, 10.0)
                .unwrap()
                .0,
            "a"
        );
        hidden.insert(ItemId::new(("a", "")));
        assert_eq!(
            nearest_sample(&plots, &transform, egui::pos2(50.0, 50.0), &hidden, 10.0)
                .unwrap()
                .0,
            "z"
        );
        assert!(nearest_sample(&plots, &transform, egui::pos2(0.0, 0.0), &hidden, 10.0).is_none());
        plots
            .apply(PlotCommand::Point {
                name: "z".into(),
                time: 95.0,
                value: 50.0,
            })
            .unwrap();
        plots.series.get_mut("z").unwrap().points[1].break_before = true;
        assert!(
            nearest_sample(&plots, &transform, egui::pos2(75.0, 50.0), &hidden, 10.0).is_none()
        );
        hidden.insert(ItemId::new(("z", "")));
        assert!(
            nearest_sample(&plots, &transform, egui::pos2(95.0, 50.0), &hidden, 10.0).is_none()
        );
        assert!(!time_axis(&plots));
    }
}
