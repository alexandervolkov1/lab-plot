use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints};
use steel::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

const MAX_POINTS: usize = 10_000;

enum PlotCommand {
    Add { name: String, unit: String },
    Point { name: String, time: f64, value: f64 },
    Clear { name: String },
}

struct PlotSeries {
    unit: String,
    points: VecDeque<[f64; 2]>,
    color: egui::Color32,
}

#[derive(Default)]
struct PlotState {
    series: BTreeMap<String, PlotSeries>,
}

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

impl PlotState {
    fn apply(&mut self, command: PlotCommand) {
        match command {
            PlotCommand::Add { name, unit } => {
                if let Some(series) = self.series.get_mut(&name) {
                    series.unit = unit;
                } else {
                    self.series.insert(
                        name,
                        PlotSeries {
                            unit,
                            points: VecDeque::new(),
                            color: series_color(self.series.len()),
                        },
                    );
                }
            }

            PlotCommand::Point { name, time, value } => {
                let color = series_color(self.series.len());

                let series = self.series.entry(name).or_insert_with(|| PlotSeries {
                    unit: String::new(),
                    points: VecDeque::new(),
                    color,
                });

                if series.points.len() >= MAX_POINTS {
                    series.points.pop_front();
                }

                series.points.push_back([time, value]);
            }

            PlotCommand::Clear { name } => {
                if let Some(series) = self.series.get_mut(&name) {
                    series.points.clear();
                }
            }
        }
    }
}

fn register_plot_functions(engine: &mut Engine, sender: Sender<PlotCommand>) {
    let tx = sender.clone();

    engine.register_fn(
        "plot/add!",
        move |name: String, unit: String| -> Result<bool, String> {
            if name.trim().is_empty() {
                return Err("Series name cannot be empty".into());
            }

            tx.send(PlotCommand::Add { name, unit })
                .map_err(|_| "Plot unavailable".to_string())?;

            Ok(true)
        },
    );

    let tx = sender.clone();

    engine.register_fn(
        "plot/point!",
        move |name: String, time: f64, value: f64| -> Result<bool, String> {
            if name.trim().is_empty() {
                return Err("Series name cannot be empty".into());
            }

            if !time.is_finite() || !value.is_finite() {
                return Err("Time and value must be finite".into());
            }

            tx.send(PlotCommand::Point { name, time, value })
                .map_err(|_| "Plot unavailable".to_string())?;

            Ok(true)
        },
    );

    engine.register_fn("plot/clear!", move |name: String| -> Result<bool, String> {
        if name.trim().is_empty() {
            return Err("Series name cannot be empty".into());
        }

        sender
            .send(PlotCommand::Clear { name })
            .map_err(|_| "Plot unavailable".to_string())?;

        Ok(true)
    });
}

struct LabPlot {
    input: String,
    history: Vec<String>,
    busy: bool,
    repl_visible: bool,
    request_tx: Sender<String>,
    response_rx: Receiver<String>,
    plot_rx: Receiver<PlotCommand>,
    plots: PlotState,
}

impl LabPlot {
    fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<String>();
        let (response_tx, response_rx) = mpsc::channel::<String>();
        let (plot_tx, plot_rx) = mpsc::channel::<PlotCommand>();

        thread::spawn(move || {
            let mut engine = Engine::new();
            register_plot_functions(&mut engine, plot_tx);

            while let Ok(source) = request_rx.recv() {
                let result = engine.compile_and_run_raw_program(source);

                let output = match result {
                    Ok(values) => {
                        let text = values
                            .into_iter()
                            .filter(|value| !matches!(value, SteelVal::Void))
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>()
                            .join("\n");

                        if text.is_empty() {
                            "(нет результата)".to_string()
                        } else {
                            text
                        }
                    }

                    Err(error) => format!("Error: {error}"),
                };

                if response_tx.send(output).is_err() {
                    break;
                }
            }
        });

        Self {
            input: String::new(),
            history: Vec::new(),
            busy: false,
            repl_visible: true,
            request_tx,
            response_rx,
            plot_rx,
            plots: PlotState::default(),
        }
    }

    fn repl_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Steel REPL");

        egui::ScrollArea::vertical()
            .max_height(270.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &self.history {
                    ui.monospace(entry);
                }
            });

        ui.separator();

        let editor = ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );

        let keyboard_submit =
            editor.has_focus() && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter));

        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled(!self.busy, egui::Button::new("Выполнить"))
                .clicked();

            if (clicked || keyboard_submit) && !self.busy && !self.input.trim().is_empty() {
                let source = std::mem::take(&mut self.input);

                self.history.push(format!("> {source}"));

                if self.request_tx.send(source).is_ok() {
                    self.busy = true;
                } else {
                    self.history.push("Steel Engine unavailable".into());
                }
            }

            if self.busy {
                ui.spinner();
            }
        });
    }
}

impl eframe::App for LabPlot {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(command) = self.plot_rx.try_recv() {
            self.plots.apply(command);
        }

        while let Ok(result) = self.response_rx.try_recv() {
            self.history.push(result);
            self.busy = false;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("lab-plot");

            if ui.button("Открыть Steel REPL").clicked() {
                self.repl_visible = true;
            }

            ui.label(format!("Кривых: {}", self.plots.series.len()));

            Plot::new("main_plot")
                .legend(Legend::default())
                .show(ui, |plot_ui| {
                    for (name, series) in &self.plots.series {
                        if series.points.is_empty() {
                            continue;
                        }

                        let points: PlotPoints = series.points.iter().copied().collect();

                        let label = if series.unit.is_empty() {
                            name.clone()
                        } else {
                            format!("{name} [{}]", series.unit)
                        };

                        plot_ui.line(Line::new(label, points).color(series.color));
                    }
                });
        });

        if self.repl_visible {
            let ctx = ui.ctx().clone();

            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("steel_repl"),
                egui::ViewportBuilder::default()
                    .with_title("Steel REPL")
                    .with_inner_size([700.0, 450.0]),
                |ui, _class| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        self.repl_visible = false;
                        return;
                    }

                    egui::CentralPanel::default().show(ui, |ui| {
                        self.repl_ui(ui);
                    });
                },
            );
        }

        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_point_and_clear_series() {
        let mut state = PlotState::default();

        state.apply(PlotCommand::Add {
            name: "sample".into(),
            unit: "degC".into(),
        });

        state.apply(PlotCommand::Point {
            name: "sample".into(),
            time: 1.0,
            value: 25.0,
        });

        assert_eq!(state.series["sample"].points.len(), 1);
        assert_eq!(state.series["sample"].unit, "degC");

        state.apply(PlotCommand::Clear {
            name: "sample".into(),
        });

        assert!(state.series["sample"].points.is_empty());
    }

    #[test]
    fn point_history_is_bounded() {
        let mut state = PlotState::default();

        for i in 0..MAX_POINTS + 5 {
            state.apply(PlotCommand::Point {
                name: "sample".into(),
                time: i as f64,
                value: i as f64,
            });
        }

        let points = &state.series["sample"].points;

        assert_eq!(points.len(), MAX_POINTS);
        assert_eq!(points.front().unwrap()[0], 5.0);
    }
}
