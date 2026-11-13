use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints, Points};
#[cfg(test)]
use lab_plot::plots::{MAX_POINTS, PlotCommand};
use lab_plot::{
    integration::{EVENTS_PER_FRAME, Integration, Phase},
    plot_display::{format_x_mark, measurement_time, nearest_sample, sample_text, time_axis},
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
    journal_visible: bool,
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
            journal_visible: false,
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
            ui.horizontal(|ui| {
                if ui.button("REPL").clicked() {
                    self.repl_visible = true;
                }
                if ui.button("Journal").clicked() {
                    self.journal_visible = true;
                }
                ui.label(self.connection.phase().name());
            });
            stream_warning(ui, &self.connection);

            let plot_id = ui.make_persistent_id("main_plot");
            let width = ui.available_width();
            let mut plot = Plot::new("main_plot")
                .id(plot_id)
                .legend(Legend::default())
                .label_formatter(|_| None);
            if time_axis(&self.plots) {
                plot = plot
                    .x_axis_formatter(move |mark, range| format_x_mark(mark, range, width))
                    .custom_x_axes(vec![
                        egui_plot::AxisHints::new_x().label_spacing(140.0..=180.0),
                    ]);
            }
            let response = plot.show(ui, |plot_ui| {
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
                        let id = (name, unit);
                        let points: PlotPoints = range.map(|i| series.points[i].xy).collect();
                        let color = series_color(series.color_index);
                        if single {
                            plot_ui
                                .points(Points::new(label, points).id(id).color(color).radius(2.0));
                        } else {
                            plot_ui.line(
                                Line::new(label, points)
                                    .id(id)
                                    .color(color)
                                    .allow_hover(false),
                            );
                        }
                    }
                }
            });
            if response.response.contains_pointer()
                && !ui.input(|i| i.pointer.any_down())
                && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
                && response.transform.frame().contains(pointer)
            {
                let hidden = egui_plot::PlotMemory::load(ui.ctx(), plot_id)
                    .map(|m| m.hidden_items)
                    .unwrap_or_default();
                if let Some((name, unit, sample)) =
                    nearest_sample(&self.plots, &response.transform, pointer, &hidden, 12.0)
                    && let Some(text) = sample_text(name, unit, sample)
                {
                    ui.painter().circle_stroke(
                        response.transform.position_from_point(&sample.xy.into()),
                        4.0,
                        egui::Stroke::new(1.5, series_color(self.plots.series[name].color_index)),
                    );
                    egui::Tooltip::always_open(
                        ui.ctx().clone(),
                        response.response.layer_id,
                        response.response.id,
                        egui::PopupAnchor::Pointer,
                    )
                    .gap(12.0)
                    .show(|ui| {
                        ui.label(text);
                    });
                }
            }
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

        if self.journal_visible {
            ui.ctx().show_viewport_immediate(
                egui::ViewportId::from_hash_of("tcp_journal"),
                egui::ViewportBuilder::default()
                    .with_title("Journal - TCP / WebSocket")
                    .with_inner_size([760.0, 560.0])
                    .with_min_inner_size([400.0, 280.0]),
                |ui, _class| {
                    journal_viewport_ui(
                        ui,
                        &self.connection,
                        &self.plots,
                        &mut self.journal_visible,
                    )
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

fn stream_warning(ui: &mut egui::Ui, connection: &Integration) {
    if let Some(error) = &connection.stream_error {
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "Stream stopped: {error}. Measurement continuity is not guaranteed."
                ))
                .color(ui.visuals().error_fg_color),
            )
            .wrap(),
        );
    }
}

fn journal_current_state(ui: &mut egui::Ui, connection: &Integration, plots: &PlotState) {
    ui.label(format!(
        "Connection {}: {}",
        connection.connection,
        connection.phase().name()
    ));
    if let Some(reason) = &connection.reason {
        ui.label(format!("Reason: {reason}"));
    }
    if connection.stream_error.is_some() {
        stream_warning(ui, connection);
    } else {
        let stream = match connection.phase() {
            Phase::Ready if connection.subscriptions.is_empty() => "Idle (no subscriptions)",
            Phase::Ready => "Listening for measurements",
            Phase::Connecting => "Waiting for connection",
            Phase::Stopping => "Stopping",
            Phase::Stopped => "Stopped",
        };
        ui.label(format!("Measurement stream: {stream}"));
    }

    ui.strong("Confirmed subscriptions");
    if connection.subscriptions.is_empty() {
        ui.weak("No confirmed subscriptions");
    }
    for key in &connection.subscriptions {
        ui.label(key);
    }

    ui.strong("Boolean signals");
    if connection.boolean_signals.is_empty() {
        ui.weak("No current Boolean observations");
    }
    for (key, state) in &connection.boolean_signals {
        let time = measurement_time(state.completed_at_unix_ms)
            .unwrap_or_else(|| "invalid timestamp".into());
        ui.label(format!(
            "{key}: {}\n{time} ({} Unix ms)",
            state.value, state.completed_at_unix_ms
        ));
    }

    ui.strong("Read errors");
    if connection.signal_errors.is_empty() {
        ui.weak("No read errors");
    }
    for (key, error) in &connection.signal_errors {
        ui.colored_label(ui.visuals().error_fg_color, format!("{key}: {error}"));
    }
    // Curve diagnostics also live in state, so they survive message eviction.
    for (name, series) in &plots.series {
        if let Some(error) = &series.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Curve {name}: {error}"),
            );
        }
    }
}

fn journal_viewport_ui(
    ui: &mut egui::Ui,
    connection: &Integration,
    plots: &PlotState,
    visible: &mut bool,
) {
    if ui.input(|i| i.viewport().close_requested()) {
        *visible = false;
        return;
    }
    egui::CentralPanel::default().show(ui, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        ui.heading("Current State");
        // Bound state height so a long subscription list cannot hide Messages.
        // Each area scrolls independently; short state leaves room for history.
        egui::ScrollArea::vertical()
            .id_salt("journal_current_state")
            .max_height(ui.available_height() * 0.45)
            .auto_shrink([false, true])
            .show(ui, |ui| journal_current_state(ui, connection, plots));
        ui.separator();
        ui.heading("Messages");
        ui.weak(format!(
            "Last {} entries; {} older entries evicted",
            lab_plot::integration::JOURNAL_CAPACITY,
            connection.evicted
        ));
        egui::ScrollArea::vertical()
            .id_salt("tcp_results")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &connection.journal {
                    let mut text = egui::RichText::new(entry.text()).monospace();
                    if entry.is_error() {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    ui.add(egui::Label::new(text).wrap().selectable(true));
                    ui.separator();
                }
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

    fn painted_text(shape: &egui::epaint::Shape, text: &mut String) {
        match shape {
            egui::epaint::Shape::Text(t) => {
                text.push_str(&t.galley.job.text);
                text.push('\n');
            }
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    painted_text(shape, text);
                }
            }
            _ => (),
        }
    }

    fn draw_text(
        ctx: &egui::Context,
        size: [f32; 2],
        mut draw: impl FnMut(&mut egui::Ui),
    ) -> String {
        let mut text = String::new();
        for _ in 0..2 {
            // A second frame settles scroll area layout.
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                ..Default::default()
            };
            let output = ctx.run_ui(input, &mut draw);
            text.clear();
            for shape in &output.shapes {
                painted_text(&shape.shape, &mut text);
            }
            output.drop_without_applying_deltas();
        }
        text
    }

    fn journal_text(app: &mut LabPlot) -> String {
        assert!(app.journal_visible);
        draw_text(&egui::Context::default(), [900.0, 1100.0], |ui| {
            journal_viewport_ui(ui, &app.connection, &app.plots, &mut app.journal_visible);
        })
    }

    fn pump_app(ctx: &egui::Context, app: &mut LabPlot, done: impl Fn(&Integration) -> bool) {
        let deadline = std::time::Instant::now() + test_support::WAIT;
        loop {
            ctx.run_ui(Default::default(), |ui| app.update_ui(ui))
                .drop_without_applying_deltas();
            if done(&app.connection) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "GUI update timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn journal_tracks_acknowledged_state_while_hidden_and_preserves_it_on_reopen() {
        use lab_plot::integration::JOURNAL_CAPACITY;
        use lab_plot::tcp_client::Request;
        use serde_json::json;
        use test_support::*;

        let (gate_tx, gate_rx) = std::sync::mpsc::sync_channel(1);
        let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            assert_eq!(
                read(&mut socket),
                json!({"id":2,"op":"subscribe","keys":["enabled","fault"]})
            );
            seen_tx.send(()).unwrap();
            gate_rx.recv_timeout(WAIT).unwrap();
            send(
                &mut socket,
                json!({"id":2,"status":"ok","keys":["enabled","fault"]}),
            );
            for key in ["enabled", "fault"] {
                send(
                    &mut socket,
                    json!({"event":"measurement","status":"ok","key":key,"value":true,"unit":"","completed_at_unix_ms":1000}),
                );
            }
            assert_eq!(read(&mut socket), json!({"id":3,"op":"ping"}));
            send(
                &mut socket,
                json!({"event":"measurement","status":"ok","key":"enabled","value":false,"unit":"","completed_at_unix_ms":2000}),
            );
            send(
                &mut socket,
                json!({"event":"measurement","status":"error","key":"fault","error":"read failed","completed_at_unix_ms":2000}),
            );
            send(&mut socket, json!({"id":3,"status":"ok"}));
            assert_eq!(
                read(&mut socket),
                json!({"id":4,"op":"unsubscribe","keys":["enabled"]})
            );
            seen_tx.send(()).unwrap();
            gate_rx.recv_timeout(WAIT).unwrap();
            send(&mut socket, json!({"id":4,"status":"ok","keys":["fault"]}));
            until_closed(&mut socket);
        });

        let ctx = egui::Context::default();
        let mut app = LabPlot::new();
        let sender = app.connection.sender();
        sender.connect(address).unwrap();
        pump_app(&ctx, &mut app, |c| c.phase() == Phase::Ready);
        sender
            .request(Request::Subscribe {
                keys: vec!["enabled".into(), "fault".into()],
            })
            .unwrap();
        seen_rx.recv_timeout(WAIT).unwrap();
        app.journal_visible = true;
        assert!(journal_text(&mut app).contains("No confirmed subscriptions"));
        gate_tx.send(()).unwrap();
        app.journal_visible = false;
        pump_app(&ctx, &mut app, |c| c.boolean_signals.len() == 2);
        let text = draw_text(&ctx, [900.0, 600.0], |ui| app.update_ui(ui));
        for hidden in ["enabled", "fault", "Boolean", "subscriptions"] {
            assert!(!text.contains(hidden), "unexpected {hidden}: {text}");
        }
        assert_eq!(app.connection.boolean_signals.len(), 2);
        app.journal_visible = true;
        let text = journal_text(&mut app);
        assert!(text.contains("Confirmed subscriptions\nenabled\nfault\nBoolean signals"));
        for expected in [
            "Connection 1: Ready",
            "Listening for measurements",
            "enabled: true",
            "fault: true",
            "1000 Unix ms",
            "Request 2",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        let history_len = app.connection.journal.len();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        ctx.run_ui(input, |ui| {
            journal_viewport_ui(ui, &app.connection, &app.plots, &mut app.journal_visible)
        })
        .drop_without_applying_deltas();
        assert!(!app.journal_visible);
        assert_eq!(app.connection.journal.len(), history_len);
        assert_eq!(app.connection.subscriptions.len(), 2);
        assert_eq!(app.connection.boolean_signals.len(), 2);
        assert_eq!(app.connection.phase(), Phase::Ready);
        assert!(!app.connection.is_closed());

        sender.request(Request::Ping).unwrap();
        pump_app(&ctx, &mut app, |c| {
            c.journal.iter().any(|e| e.request_id() == Some(3))
        });
        assert!(!app.journal_visible);
        assert!(!app.connection.boolean_signals["enabled"].value);
        assert_eq!(app.connection.signal_errors["fault"], "read failed");
        app.journal_visible = true;
        let text = journal_text(&mut app);
        for expected in [
            "enabled: false",
            "2000 Unix ms",
            "fault: read failed",
            "Request 2",
            "Request 3",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("fault: true"));

        // Evict the subscribe response through the normal bounded journal path.
        app.journal_visible = false;
        for _ in 0..JOURNAL_CAPACITY {
            sender
                .plot(PlotCommand::Bind {
                    name: "missing".into(),
                    key: "unused".into(),
                })
                .unwrap();
        }
        pump_app(&ctx, &mut app, |c| c.evicted > 0);
        assert_eq!(app.connection.journal.len(), JOURNAL_CAPACITY);
        assert!(
            app.connection
                .journal
                .iter()
                .all(|e| e.request_id() != Some(2))
        );
        app.journal_visible = true;
        let text = journal_text(&mut app);
        assert!(text.contains("Confirmed subscriptions\nenabled\nfault\nBoolean signals"));
        assert!(text.contains("enabled: false"));
        assert!(text.contains("fault: read failed"));
        assert!(text.contains(&format!("{} older entries evicted", app.connection.evicted)));

        sender
            .request(Request::Unsubscribe {
                keys: vec!["enabled".into()],
            })
            .unwrap();
        seen_rx.recv_timeout(WAIT).unwrap();
        assert!(journal_text(&mut app).contains("enabled: false"));
        gate_tx.send(()).unwrap();
        app.journal_visible = false;
        pump_app(&ctx, &mut app, |c| !c.subscriptions.contains("enabled"));
        app.journal_visible = true;
        let text = journal_text(&mut app);
        assert!(text.contains("Confirmed subscriptions\nfault\nBoolean signals"));
        assert!(text.contains("No current Boolean observations"));
        assert!(text.contains("fault: read failed"));
        assert_eq!(
            app.connection
                .subscriptions
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["fault"]
        );

        app.journal_visible = false;
        sender.disconnect().unwrap();
        pump_app(&ctx, &mut app, |c| c.phase() == Phase::Stopped);
        assert!(app.connection.subscriptions.is_empty());
        assert!(app.connection.boolean_signals.is_empty());
        // Existing disconnect rules retain read errors until a fresh connection.
        assert_eq!(app.connection.signal_errors["fault"], "read failed");
        app.journal_visible = true;
        let text = journal_text(&mut app);
        for expected in [
            "Connection 1: Stopped",
            "No confirmed subscriptions",
            "No current Boolean observations",
            "Measurement stream: Stopped",
            "Reason: Shutdown",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!app.connection.is_closed());
        server.join().unwrap();
    }

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
    fn journal_displays_current_values_timestamps_and_errors_without_messages() {
        let ctx = egui::Context::default();
        let mut connection = Integration::default();
        let mut plots = PlotState::default();
        plots
            .apply(PlotCommand::Add {
                name: "temperature".into(),
                unit: "K".into(),
            })
            .unwrap();
        plots.series.get_mut("temperature").unwrap().error = Some("unit mismatch".into());
        connection.reason = Some("connection details".into());
        connection.subscriptions.insert("enabled".into());
        for (key, value) in [("enabled", true), ("fault", false)] {
            connection.boolean_signals.insert(
                key.into(),
                lab_plot::integration::BooleanState {
                    value,
                    completed_at_unix_ms: 1234,
                },
            );
        }
        let draw = |connection: &Integration| {
            draw_text(&ctx, [900.0, 1100.0], |ui| {
                journal_viewport_ui(ui, connection, &plots, &mut true);
            })
        };
        let text = draw(&connection);
        for expected in [
            "Current State",
            "Messages",
            "Confirmed subscriptions",
            "Boolean signals",
            "enabled: true",
            "fault: false",
            "1234 Unix ms",
            "Reason: connection details",
            "Curve temperature: unit mismatch",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(text.contains(&measurement_time(1234).unwrap()));
        assert!(connection.journal.is_empty());
        connection.boolean_signals.remove("enabled");
        connection
            .signal_errors
            .insert("enabled".into(), "read failed".into());
        let text = draw(&connection);
        assert!(!text.contains("enabled: true"));
        assert!(text.contains("enabled: read failed"));
        connection.begin_close();
        let text = draw(&connection);
        assert!(text.contains("No current Boolean observations"));
        assert!(!text.contains("fault: false"));
        connection.stream_error = Some("observation_data_lost".into());
        let text = draw(&connection);
        assert!(text.contains("Stream stopped: observation_data_lost"));
        assert!(text.contains("Measurement continuity is not guaranteed"));
    }

    #[test]
    fn main_window_only_shows_controls_status_and_critical_stream_warnings() {
        use lab_plot::integration::{JournalEntry, JournalResult};
        let ctx = egui::Context::default();
        let mut app = LabPlot::new();
        app.connection
            .subscriptions
            .insert("subscription_only".into());
        app.connection
            .signal_errors
            .insert("failed_parameter".into(), "read failed".into());
        app.connection.reason = Some("ordinary connection details".into());
        app.connection.journal.push_back(JournalEntry {
            connection: 1,
            received_at: std::time::SystemTime::now(),
            result: JournalResult::Message("history only".into()),
        });
        app.plots
            .apply(PlotCommand::Add {
                name: "curve".into(),
                unit: "K".into(),
            })
            .unwrap();
        app.plots.series.get_mut("curve").unwrap().error = Some("curve read error".into());
        for error in [
            None,
            Some("observation_data_lost"),
            Some("observation_stream_closed"),
            Some("EventQueueOverflow"),
        ] {
            app.connection.stream_error = error.map(str::to_owned);
            let text = draw_text(&ctx, [900.0, 600.0], |ui| app.update_ui(ui));
            for expected in ["REPL", "Journal", "Stopped"] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            for hidden in [
                "Subscriptions",
                "subscription_only",
                "Boolean",
                "failed_parameter",
                "read failed",
                "curve read error",
                "ordinary connection details",
                "history only",
                "Server results and subscriptions",
            ] {
                assert!(!text.contains(hidden), "unexpected {hidden}: {text}");
            }
            if let Some(error) = error {
                assert!(text.contains(error));
                assert!(text.contains("Measurement continuity is not guaranteed"));
            } else {
                assert!(!text.contains("Stream stopped"));
            }
        }
    }

    #[test]
    fn journal_keeps_both_areas_visible_when_resized_with_many_subscriptions() {
        use lab_plot::integration::{JournalEntry, JournalResult};
        let ctx = egui::Context::default();
        apply_mirage_theme(&ctx);
        let mut connection = Integration::default();
        connection
            .subscriptions
            .extend((0..100).map(|i| format!("instrument/very/long/parameter/name/{i}")));
        connection.journal.push_back(JournalEntry {
            connection: 1,
            received_at: std::time::SystemTime::now(),
            result: JournalResult::Message("history remains visible".into()),
        });
        for size in [[760.0, 560.0], [400.0, 280.0], [1000.0, 800.0]] {
            let text = draw_text(&ctx, size, |ui| {
                journal_viewport_ui(ui, &connection, &PlotState::default(), &mut true);
            });
            for expected in [
                "Current State",
                "Messages",
                "Connection 0: Stopped",
                "history remains visible",
            ] {
                assert!(
                    text.contains(expected),
                    "size {size:?}, missing {expected}: {text}"
                );
            }
        }
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
