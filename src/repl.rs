use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;

pub struct Repl {
    input: String,
    history: Vec<String>,
    busy: bool,
    request_tx: Sender<String>,
    response_rx: Receiver<String>,
}

impl Repl {
    pub fn new(configure: impl FnOnce(&mut Engine) + Send + 'static) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<String>();
        let (response_tx, response_rx) = mpsc::channel::<String>();

        thread::spawn(move || {
            let mut engine = Engine::new();

            configure(&mut engine);

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
            request_tx,
            response_rx,
        }
    }

    pub fn poll(&mut self) {
        while let Ok(result) = self.response_rx.try_recv() {
            self.history.push(result);
            self.busy = false;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
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
