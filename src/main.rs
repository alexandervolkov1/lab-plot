use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints, Points};
#[cfg(test)]
use lab_plot::plots::{MAX_POINTS, PlotCommand};
use lab_plot::{
    integration::{EVENTS_PER_FRAME, Integration},
    plots::PlotState,
    steel_api::{register_lab_functions, register_plot_functions},
};
use std::time::Duration;
mod repl;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
fn series_color(index: usize) -> egui::Color32 {
    use egui::Color32;

    let colors = [
        Color32::from_rgb(115, 208, 255), // голубой
        Color32::from_rgb(186, 230, 126), // зелёный
        Color32::from_rgb(255, 204, 102), // жёлтый
        Color32::from_rgb(212, 191, 255), // фиолетовый
        Color32::from_rgb(242, 135, 121), // красный
    ];

    colors[index % colors.len()]
}

struct LabPlot {
    repl: repl::Repl,
    repl_visible: bool,
    connection: Integration,
    plots: PlotState,
}

impl LabPlot {
    fn new() -> Self {
        let connection = Integration::default();
        let sender = connection.sender();
        let repl = repl::Repl::new(move |engine| {
            register_plot_functions(engine, sender.clone());
            register_lab_functions(engine, sender);
        });

        Self {
            repl,
            repl_visible: false,
            connection,
            plots: PlotState::default(),
        }
    }
}

impl eframe::App for LabPlot {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_ui(ui);
    }
}

impl LabPlot {
    fn update_ui(&mut self, ui: &mut egui::Ui) {
        let processed = self.connection.update(&mut self.plots);
        self.repl.poll();
        if self.repl.close_ui(ui) {
            self.connection.begin_close();
            if !self.connection.is_closed() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ui.ctx().request_repaint();
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if ui.button("REPL").clicked() {
                self.repl_visible = true;
            }
            connection_status(ui, &self.connection, &self.plots);

            Plot::new("main_plot")
                .legend(Legend::default())
                .show(ui, |plot_ui| {
                    for (name, series) in &self.plots.series {
                        if series.points.is_empty() {
                            continue;
                        }

                        for range in series.segments() {
                            let unit = series.sample_unit(range.start);
                            let label = if unit.is_empty() {
                                name.clone()
                            } else {
                                format!("{name} [{unit}]")
                            };
                            let single = range.len() == 1;
                            let id = (name, range.start);
                            let points: PlotPoints = range.map(|i| series.points[i].xy).collect();
                            let color = series_color(series.color_index);
                            if single {
                                plot_ui.points(
                                    Points::new(label, points).id(id).color(color).radius(2.0),
                                );
                            } else {
                                plot_ui.line(Line::new(label, points).id(id).color(color));
                            }
                        }
                    }
                });
        });

        if self.repl_visible {
            let ctx = ui.ctx().clone();

            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("steel_repl"),
                egui::ViewportBuilder::default()
                    .with_title("Steel")
                    .with_inner_size([800.0, 640.0])
                    .with_min_inner_size([520.0, 440.0]),
                |ui, _class| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        self.repl_visible = false;
                        return;
                    }

                    egui::CentralPanel::default().show(ui, |ui| {
                        self.repl.ui(ui);
                    });
                },
            );
        }

        if processed == EVENTS_PER_FRAME {
            ui.ctx().request_repaint();
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn connection_status(ui: &mut egui::Ui, connection: &Integration, plots: &PlotState) {
    ui.label(format!(
        "Connection {}: {}",
        connection.connection,
        connection.phase().name()
    ));
    if let Some(reason) = &connection.reason {
        ui.label(format!("Reason: {reason}"));
    }
    if let Some(error) = &connection.stream_error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Stream stopped: {error}. Measurement continuity is not guaranteed."),
        );
    }
    egui::CollapsingHeader::new("TCP results and subscriptions").default_open(true).show(ui, |ui| {
        ui.label(format!("Subscriptions: {}", connection.subscriptions.iter().cloned().collect::<Vec<_>>().join(", ")));
        ui.label("Boolean signals");
        if connection.boolean_signals.is_empty() { ui.weak("No current Boolean observations"); }
        for (key, state) in &connection.boolean_signals {
            ui.label(format!("{key}    {}    ({} Unix ms)", state.value, state.completed_at_unix_ms));
        }
        for (key, error) in &connection.signal_errors { ui.colored_label(ui.visuals().error_fg_color, format!("{key}: {error}")); }
        for (name, series) in &plots.series {
            if let Some(error) = &series.error { ui.colored_label(ui.visuals().error_fg_color, format!("{name}: {error}")); }
        }
        ui.weak(format!("Last 200 results/errors; {} older entries evicted", connection.evicted));
        ui.weak("Request ID = accepted, not executed. OutcomeUnknown / outcome_unknown = execution may have occurred; no automatic retry.");
        egui::ScrollArea::vertical().id_salt("tcp_results").max_height(130.0).stick_to_bottom(true).show(ui, |ui| {
            for entry in &connection.journal { ui.label(entry.text()); }
        });
    });
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "lab-plot",
        options,
        Box::new(|cc| {
            apply_mirage_theme(&cc.egui_ctx);
            Ok(Box::new(LabPlot::new()))
        }),
    )
}

fn apply_mirage_theme(ctx: &egui::Context) {
    use egui::{Color32, Stroke, Visuals};

    let background = Color32::from_rgb(31, 36, 48);
    let surface = Color32::from_rgb(36, 41, 54);
    let text = Color32::from_rgb(204, 202, 194);
    let muted = Color32::from_rgb(112, 122, 140);
    let accent = Color32::from_rgb(255, 204, 102);
    let border = Color32::from_rgb(50, 56, 68);

    let mut visuals = Visuals::dark();

    visuals.panel_fill = background;
    visuals.window_fill = background;
    visuals.faint_bg_color = surface;
    visuals.extreme_bg_color = Color32::from_rgb(27, 32, 43);
    visuals.text_edit_bg_color = Some(background);
    visuals.code_bg_color = surface;

    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, accent);

    visuals.widgets.inactive.weak_bg_fill = surface;
    visuals.widgets.inactive.bg_fill = surface;
    visuals.widgets.hovered.weak_bg_fill = border;
    visuals.widgets.hovered.bg_fill = border;

    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    visuals.window_stroke = Stroke::new(1.0, border);

    visuals.weak_text_color = Some(muted);
    visuals.selection.bg_fill = Color32::from_rgb(57, 68, 88);
    visuals.selection.stroke = Stroke::new(1.0, accent);
    visuals.hyperlink_color = Color32::from_rgb(115, 208, 255);
    visuals.warn_fg_color = accent;
    visuals.error_fg_color = Color32::from_rgb(242, 135, 121);

    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));

        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));

        style
            .text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(16.0));

        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(21.0));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_defers_window_close_until_client_has_finished() {
        let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
        let (address, server) = test_support::mock(move |mut socket| {
            test_support::hello(&mut socket);
            seen_tx.send(()).unwrap();
            test_support::until_closed(&mut socket);
        });
        let ctx = egui::Context::default();
        let mut app = LabPlot::new();
        app.connection.sender().connect(address).unwrap();
        ctx.run_ui(egui::RawInput::default(), |ui| app.update_ui(ui))
            .drop_without_applying_deltas();
        seen_rx.recv_timeout(test_support::WAIT).unwrap();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run_ui(input, |ui| app.update_ui(ui));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        output.drop_without_applying_deltas();
        let deadline = std::time::Instant::now() + test_support::WAIT;
        loop {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| app.update_ui(ui));
            if app.connection.is_closed() {
                assert!(
                    output.viewport_output[&egui::ViewportId::ROOT]
                        .commands
                        .contains(&egui::ViewportCommand::Close)
                );
                assert!(
                    !output.viewport_output[&egui::ViewportId::ROOT]
                        .commands
                        .contains(&egui::ViewportCommand::CancelClose)
                );
                output.drop_without_applying_deltas();
                break;
            }
            output.drop_without_applying_deltas();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        server.join().unwrap();
    }

    #[test]
    fn boolean_status_panel_displays_values_timestamps_and_read_errors() {
        let ctx = egui::Context::default();
        let mut connection = Integration::default();
        let plots = PlotState::default();
        for (key, value) in [("enabled", true), ("fault", false)] {
            connection.boolean_signals.insert(
                key.into(),
                lab_plot::integration::BooleanState {
                    value,
                    completed_at_unix_ms: 1234,
                },
            );
        }
        fn painted_text(shape: &egui::epaint::Shape, text: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => text.push_str(&t.galley.job.text),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        painted_text(shape, text);
                    }
                }
                _ => (),
            }
        }
        let draw = |connection: &Integration| {
            let mut text = String::new();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                connection_status(ui, connection, &plots)
            });
            for shape in &output.shapes {
                painted_text(&shape.shape, &mut text);
            }
            output.drop_without_applying_deltas();
            text
        };
        draw(&connection); // initial layout
        let text = draw(&connection);
        assert!(text.contains("Boolean signals"));
        assert!(text.contains("enabled    true    (1234 Unix ms)"));
        assert!(text.contains("fault    false    (1234 Unix ms)"));
        connection.boolean_signals.remove("enabled");
        connection
            .signal_errors
            .insert("enabled".into(), "read failed".into());
        let text = draw(&connection);
        assert!(!text.contains("enabled    true"));
        assert!(text.contains("enabled: read failed"));
        connection.begin_close();
        let text = draw(&connection);
        assert!(text.contains("No current Boolean observations"));
        assert!(!text.contains("fault    false"));
    }

    #[test]
    fn headless_gui_draws_discontinuous_curves_and_handles_close() {
        use lab_plot::tcp_client::{Measurement, Reading, ScalarValue};
        let ctx = egui::Context::default();
        let mut app = LabPlot::new();
        app.plots
            .apply(PlotCommand::Add {
                name: "A".into(),
                unit: "degC".into(),
            })
            .unwrap();
        app.plots
            .apply(PlotCommand::Bind {
                name: "A".into(),
                key: "a".into(),
            })
            .unwrap();
        for time in 0..3 {
            app.plots.measurement(Measurement {
                key: "a".into(),
                completed_at_unix_ms: time * 1000,
                reading: if time == 1 {
                    Reading::Error("read failed".into())
                } else {
                    Reading::Value {
                        value: ScalarValue::Float(20.0),
                        unit: "degC".into(),
                    }
                },
            });
        }
        app.connection.stream_error = Some("observation_data_lost".into());
        ctx.run_ui(egui::RawInput::default(), |ui| app.update_ui(ui))
            .drop_without_applying_deltas();
        assert_eq!(app.plots.series["A"].segments(), vec![0..1, 1..2]);
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        ctx.run_ui(input, |ui| app.update_ui(ui))
            .drop_without_applying_deltas();
        assert!(app.connection.is_closed());
    }

    #[test]
    fn add_point_and_clear_series() {
        let mut state = PlotState::default();

        state
            .apply(PlotCommand::Add {
                name: "sample".into(),
                unit: "degC".into(),
            })
            .unwrap();

        state
            .apply(PlotCommand::Point {
                name: "sample".into(),
                time: 1.0,
                value: 25.0,
            })
            .unwrap();

        assert_eq!(state.series["sample"].points.len(), 1);
        assert_eq!(state.series["sample"].unit, "degC");

        state
            .apply(PlotCommand::Clear {
                name: "sample".into(),
            })
            .unwrap();

        assert!(state.series["sample"].points.is_empty());
    }

    #[test]
    fn point_history_is_bounded() {
        let mut state = PlotState::default();

        for i in 0..MAX_POINTS + 5 {
            state
                .apply(PlotCommand::Point {
                    name: "sample".into(),
                    time: i as f64,
                    value: i as f64,
                })
                .unwrap();
        }

        let points = &state.series["sample"].points;

        assert_eq!(points.len(), MAX_POINTS);
        assert_eq!(points.front().unwrap().xy[0], 5.0);
    }
}
