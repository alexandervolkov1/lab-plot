use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;

mod highlight;
mod indent;

const MAX_COMMAND_HISTORY: usize = 200;

#[derive(Default)]
struct CommandHistory {
    commands: VecDeque<String>,
    position: Option<usize>,
    draft: String,
}

impl CommandHistory {
    fn record(&mut self, source: &str) {
        if self.commands.back().map(String::as_str) != Some(source) {
            if self.commands.len() == MAX_COMMAND_HISTORY {
                self.commands.pop_front();
            }

            self.commands.push_back(source.to_owned());
        }

        self.position = None;
        self.draft.clear();
    }

    fn previous(&mut self, input: &mut String) -> bool {
        if self.commands.is_empty() {
            return false;
        }

        let index = match self.position {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft.clone_from(input);
                self.commands.len() - 1
            }
        };

        self.position = Some(index);
        input.clone_from(&self.commands[index]);

        true
    }

    fn next(&mut self, input: &mut String) -> bool {
        let Some(index) = self.position else {
            return false;
        };

        if index + 1 < self.commands.len() {
            let next = index + 1;
            self.position = Some(next);
            input.clone_from(&self.commands[next]);
        } else {
            self.position = None;
            *input = std::mem::take(&mut self.draft);
        }

        true
    }
}

pub struct Repl {
    input: String,
    history: Vec<String>,
    busy: bool,
    request_tx: Sender<String>,
    response_rx: Receiver<String>,
    command_history: CommandHistory,
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
            command_history: CommandHistory::default(),
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
                    ui.add(
                        egui::Label::new(egui::RichText::new(entry).monospace()).selectable(true),
                    );
                }
            });

        ui.separator();

        let editor_id = ui.make_persistent_id("steel_repl_input");

        let focused =
            ui.memory(|memory| memory.has_focus(editor_id)) && ui.input(|input| input.focused);

        let mut history_moved = false;

        if focused {
            let (previous, next) = ui.input_mut(|input| {
                if input.modifiers.matches_exact(egui::Modifiers::CTRL) {
                    (
                        input.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp),
                        input.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown),
                    )
                } else {
                    (false, false)
                }
            });

            if previous {
                history_moved = self.command_history.previous(&mut self.input);
            } else if next {
                history_moved = self.command_history.next(&mut self.input);
            }
        }

        let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = highlight::highlight(text.as_str());
            job.wrap.max_width = wrap_width;
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };

        if focused {
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) {
                if let Some(cursor_range) = state.cursor.char_range() {
                    let enter = ui.input_mut(|input| {
                        input.modifiers.matches_exact(egui::Modifiers::NONE)
                            && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });

                    if enter {
                        let range = cursor_range.as_sorted_char_range();

                        let new_cursor =
                            indent::insert_newline(&mut self.input, range.start.0, range.end.0);

                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(new_cursor),
                            )));

                        state.store(ui.ctx(), editor_id);
                    }
                }
            }
        }

        let editor = ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .id(editor_id)
                .font(egui::TextStyle::Monospace)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .layouter(&mut layouter),
        );

        if history_moved {
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) {
                let cursor = egui::text::CCursor::new(self.input.chars().count());

                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(cursor)));

                state.store(ui.ctx(), editor_id);
            }
        }

        let keyboard_submit =
            editor.has_focus() && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter));

        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled(!self.busy, egui::Button::new("Execute"))
                .clicked();

            if (clicked || keyboard_submit) && !self.busy && !self.input.trim().is_empty() {
                let source = std::mem::take(&mut self.input);

                self.history.push(format!("> {source}"));

                if self.request_tx.send(source.clone()).is_ok() {
                    self.command_history.record(&source);
                    self.busy = true;
                } else {
                    self.history.push("Steel Engine unavailable".into());
                    self.input = source;
                }
            }

            if self.busy {
                ui.spinner();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_navigation_restores_draft() {
        let mut history = CommandHistory::default();

        history.record("(+ 1 2)");
        history.record("(+ 3 4)");

        let mut input = "(define x 10)".to_owned();

        assert!(history.previous(&mut input));
        assert_eq!(input, "(+ 3 4)");

        assert!(history.previous(&mut input));
        assert_eq!(input, "(+ 1 2)");

        assert!(history.previous(&mut input));
        assert_eq!(input, "(+ 1 2)");

        assert!(history.next(&mut input));
        assert_eq!(input, "(+ 3 4)");

        assert!(history.next(&mut input));
        assert_eq!(input, "(define x 10)");

        assert!(!history.next(&mut input));
    }

    #[test]
    fn history_skips_consecutive_duplicates() {
        let mut history = CommandHistory::default();

        history.record("(+ 1 2)");
        history.record("(+ 1 2)");

        assert_eq!(history.commands.len(), 1);
    }

    #[test]
    fn history_has_bounded_size() {
        let mut history = CommandHistory::default();

        for i in 0..205 {
            history.record(&format!("(+ {i} 1)"));
        }

        assert_eq!(history.commands.len(), 200);
        assert_eq!(history.commands.front().unwrap(), "(+ 5 1)");
        assert_eq!(history.commands.back().unwrap(), "(+ 204 1)");
    }

    #[test]
    fn empty_history_preserves_input() {
        let mut history = CommandHistory::default();
        let mut input = "(define x 42)".to_owned();

        assert!(!history.previous(&mut input));
        assert_eq!(input, "(define x 42)");
    }
}
