use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;

mod files;
mod highlight;
mod indent;

use files::ScriptFile;

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

struct EvalRequest {
    source: String,
    path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum FileAction {
    New,
    Open,
}

pub struct Repl {
    script: ScriptFile,
    input: String,
    history: Vec<String>,
    last_output: String,
    file_error: Option<String>,
    pending_action: Option<FileAction>,
    reset_script_cursor: bool,
    busy: bool,
    request_tx: Sender<EvalRequest>,
    response_rx: Receiver<String>,
    command_history: CommandHistory,
}

impl Repl {
    pub fn new(configure: impl FnOnce(&mut Engine) + Send + 'static) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<EvalRequest>();
        let (response_tx, response_rx) = mpsc::channel::<String>();

        thread::spawn(move || {
            let mut engine = Engine::new();
            configure(&mut engine);
            while let Ok(request) = request_rx.recv() {
                let result = match request.path {
                    Some(path) => {
                        engine.compile_and_run_raw_program_with_path(request.source, path)
                    }
                    None => engine.compile_and_run_raw_program(request.source),
                };
                let output = match result {
                    Ok(values) => {
                        let text = values
                            .into_iter()
                            .filter(|value| !matches!(value, SteelVal::Void))
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            "(нет результата)".into()
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
            script: ScriptFile::default(),
            input: String::new(),
            history: Vec::new(),
            last_output: String::new(),
            file_error: None,
            pending_action: None,
            reset_script_cursor: false,
            busy: false,
            request_tx,
            response_rx,
            command_history: CommandHistory::default(),
        }
    }

    pub fn poll(&mut self) {
        loop {
            match self.response_rx.try_recv() {
                Ok(result) => {
                    self.last_output.clone_from(&result);
                    self.history.push(result);
                    self.busy = false;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.busy {
                        self.last_output = "Steel Engine unavailable".into();
                        self.history.push(self.last_output.clone());
                        self.busy = false;
                    }
                    break;
                }
            }
        }
    }

    fn submit(&mut self, source: String, path: Option<PathBuf>) -> bool {
        if self.busy || source.trim().is_empty() {
            return false;
        }
        let entry = format!("> {source}");
        if self.request_tx.send(EvalRequest { source, path }).is_err() {
            self.last_output = "Steel Engine unavailable".into();
            return false;
        }
        self.history.push(entry);
        self.busy = true;
        true
    }

    fn run_script(&mut self) {
        self.submit(self.script.text.clone(), self.script.path.clone());
    }

    fn execute_input(&mut self) {
        let source = self.input.clone();
        if self.submit(source.clone(), None) {
            self.command_history.record(&source);
            self.input.clear();
        }
    }

    fn request_file_action(&mut self, action: FileAction) {
        if self.script.dirty() {
            self.pending_action = Some(action);
        } else {
            self.perform_file_action(action);
        }
    }

    fn perform_file_action(&mut self, action: FileAction) {
        match action {
            FileAction::New => {
                self.script = ScriptFile::default();
                self.file_error = None;
                self.reset_script_cursor = true;
            }
            FileAction::Open => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Steel", &["scm", "ss"])
                    .pick_file()
                {
                    match self.script.open(&path) {
                        Ok(()) => {
                            self.file_error = None;
                            self.reset_script_cursor = true;
                        }
                        Err(error) => self.file_error = Some(format!("Open: {error}")),
                    }
                }
            }
        }
    }

    fn save(&mut self, save_as: bool) -> bool {
        let path = if save_as || self.script.path.is_none() {
            let mut dialog = rfd::FileDialog::new().add_filter("Steel", &["scm", "ss"]);
            if let Some(path) = &self.script.path {
                if let Some(parent) = path.parent() {
                    dialog = dialog.set_directory(parent);
                }
                dialog = dialog.set_file_name(path.file_name().unwrap().to_string_lossy());
            } else {
                dialog = dialog.set_file_name("experiment.scm");
            }
            dialog.save_file()
        } else {
            self.script.path.clone()
        };
        let Some(path) = path else {
            return false;
        };
        match self.script.save_as(&path) {
            Ok(()) => {
                self.file_error = None;
                true
            }
            Err(error) => {
                self.file_error = Some(format!("Save: {error}"));
                false
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.pending_action.is_none() && ui.input(|i| i.focused) {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
                self.request_file_action(FileAction::New);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
                self.request_file_action(FileAction::Open);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
                self.save(false);
            }
        }

        ui.add_enabled_ui(self.pending_action.is_none(), |ui| {
            ui.horizontal(|ui| {
                if ui.button("New").clicked() {
                    self.request_file_action(FileAction::New);
                }
                if ui.button("Open").clicked() {
                    self.request_file_action(FileAction::Open);
                }
                if ui.button("Save").clicked() {
                    self.save(false);
                }
                if ui.button("Save As").clicked() {
                    self.save(true);
                }
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Run"))
                    .clicked()
                {
                    self.run_script();
                }
            });
        });
        let name = self
            .script
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| "Untitled.scm".into());
        ui.label(format!(
            "{name}{}",
            if self.script.dirty() { " *" } else { "" }
        ))
        .on_hover_text("Для require относительно каталога скрипта сначала сохраните файл.");
        if let Some(error) = &self.file_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(action) = self.pending_action {
            ui.label("Несохранённые изменения. Сохранить перед продолжением?");
            ui.horizontal(|ui| {
                if ui.button("Save and continue").clicked() && self.save(false) {
                    self.pending_action = None;
                    self.perform_file_action(action);
                }
                if ui.button("Discard and continue").clicked() {
                    self.pending_action = None;
                    self.perform_file_action(action);
                }
                if ui.button("Cancel").clicked() {
                    self.pending_action = None;
                }
            });
        }

        ui.add_enabled_ui(self.pending_action.is_none(), |ui| {
            let height = ui.available_height();
            egui::Panel::bottom("steel_repl_panel")
                .resizable(true)
                .default_size(height * 0.3)
                .min_size(140.0)
                .max_size((height - 100.0).max(140.0))
                .show(ui, |ui| self.repl_ui(ui));
            egui::CentralPanel::default().show(ui, |ui| {
                let id = ui.make_persistent_id("steel_script_editor");
                if self.reset_script_cursor {
                    egui::text_edit::TextEditState::default().store(ui.ctx(), id);
                    self.reset_script_cursor = false;
                }
                let submit = egui::ScrollArea::vertical()
                    .id_salt("script_scroll")
                    .show(ui, |ui| {
                        code_editor(ui, id, &mut self.script.text, ui.available_height(), None)
                    })
                    .inner;
                if submit {
                    self.run_script();
                }
            });
        });
    }

    fn repl_ui(&mut self, ui: &mut egui::Ui) {
        ui.set_min_height(ui.available_height());
        ui.strong("REPL");
        let id = ui.make_persistent_id("steel_repl_input");
        let focused = ui.is_enabled() && ui.memory(|m| m.has_focus(id)) && ui.input(|i| i.focused);
        let mut moved = false;
        if focused {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp)) {
                moved = self.command_history.previous(&mut self.input);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown)) {
                moved = self.command_history.next(&mut self.input);
            }
        }
        let cursor = moved.then(|| self.input.chars().count());
        let input_height = (ui.available_height() * 0.45).max(40.0);
        let submit = egui::ScrollArea::vertical()
            .id_salt("repl_input_scroll")
            .max_height(input_height)
            .show(ui, |ui| {
                code_editor(ui, id, &mut self.input, input_height, cursor)
            })
            .inner;
        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled(!self.busy, egui::Button::new("Execute"))
                .clicked();
            if clicked || submit {
                self.execute_input();
            }
            if self.busy {
                ui.spinner();
            }
            ui.weak("Ctrl+Enter · Ctrl+PageUp/PageDown");
        });
        // Only the latest result is visible; earlier commands/results remain in RAM.
        egui::ScrollArea::vertical()
            .id_salt("latest_result_scroll")
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&self.last_output).monospace())
                        .selectable(true),
                );
            });
    }
}

/// Shared editing behavior, with distinct buffers, widget IDs and undo states.
fn code_editor(
    ui: &mut egui::Ui,
    id: egui::Id,
    text: &mut String,
    height: f32,
    cursor: Option<usize>,
) -> bool {
    let focused = ui.is_enabled() && ui.memory(|m| m.has_focus(id)) && ui.input(|i| i.focused);
    let submit =
        focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter));
    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
        if let Some(cursor) = cursor {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
        } else if focused
            && let Some(range) = state.cursor.char_range()
            && ui.input_mut(|i| {
                i.modifiers.matches_exact(egui::Modifiers::NONE)
                    && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            })
        {
            let range = range.as_sorted_char_range();
            let cursor = indent::insert_newline(text, range.start.0, range.end.0);
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
        }
        state.store(ui.ctx(), id);
    }
    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
        let mut job = highlight::highlight(text.as_str());
        job.wrap.max_width = width;
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    };
    ui.add(
        egui::TextEdit::multiline(text)
            .id(id)
            .font(egui::TextStyle::Monospace)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .min_size(egui::vec2(0.0, height.max(40.0)))
            .layouter(&mut layouter),
    );
    submit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, repl: &mut Repl, events: Vec<egui::Event>) -> egui::FullOutput {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 700.0),
                )),
                focused: true,
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| repl.ui(ui));
            },
        )
    }

    fn ctrl(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                command: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn ui_file_shortcuts_save_and_require_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyboard.scm");
        let mut repl = Repl::new(|_| {});
        repl.script.save_as(&path).unwrap();
        repl.script.text = "; несохранённый скрипт".into();
        let ctx = egui::Context::default();
        frame(&ctx, &mut repl, vec![]).drop_without_applying_deltas();
        frame(&ctx, &mut repl, vec![ctrl(egui::Key::S)]).drop_without_applying_deltas();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repl.script.text);
        assert!(!repl.script.dirty());
        repl.script.text.push_str("\n; edit");
        frame(&ctx, &mut repl, vec![ctrl(egui::Key::N)]).drop_without_applying_deltas();
        assert!(matches!(repl.pending_action, Some(FileAction::New)));
        repl.pending_action = None;
        frame(&ctx, &mut repl, vec![ctrl(egui::Key::O)]).drop_without_applying_deltas();
        assert!(matches!(repl.pending_action, Some(FileAction::Open)));
        assert!(repl.script.text.ends_with("; edit"));
    }

    #[test]
    fn ui_renders_only_latest_result_and_preserves_state_when_reopened() {
        fn visible_text(shape: &egui::epaint::Shape, text: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => text.push_str(&t.galley.job.text),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        visible_text(shape, text);
                    }
                }
                _ => {}
            }
        }
        let mut repl = Repl::new(|_| {});
        repl.script.text = "(define retained 42)".into();
        repl.input = "unfinished input".into();
        repl.history.push("PREVIOUS_RESULT_MUST_BE_HIDDEN".into());
        repl.last_output = "LATEST_RESULT_VISIBLE".into();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            // Hide/reopen keeps this same Repl, exactly as LabPlot does.
            let output = frame(&ctx, &mut repl, vec![]);
            let mut text = String::new();
            for shape in &output.shapes {
                visible_text(&shape.shape, &mut text);
            }
            assert!(text.contains("LATEST_RESULT_VISIBLE"));
            assert!(!text.contains("PREVIOUS_RESULT_MUST_BE_HIDDEN"));
            assert_eq!(repl.script.text, "(define retained 42)");
            assert_eq!(repl.input, "unfinished input");
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn repl_panel_can_be_resized_by_dragging_the_separator() {
        fn separator(output: &egui::FullOutput) -> f32 {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::epaint::Shape::LineSegment { points, .. } = &shape.shape
                        && points[0].y == points[1].y
                        && (points[0].x - points[1].x).abs() > 600.0
                    {
                        Some(points[0].y)
                    } else {
                        None
                    }
                })
                .expect("panel separator")
        }
        let ctx = egui::Context::default();
        let mut repl = Repl::new(|_| {});
        let output = frame(&ctx, &mut repl, vec![]);
        let initial_y = separator(&output);
        output.drop_without_applying_deltas();
        let start = egui::pos2(400.0, initial_y);
        let end = start - egui::vec2(0.0, 80.0);
        frame(&ctx, &mut repl, vec![egui::Event::PointerMoved(start)])
            .drop_without_applying_deltas();
        frame(
            &ctx,
            &mut repl,
            vec![egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
        )
        .drop_without_applying_deltas();
        frame(&ctx, &mut repl, vec![egui::Event::PointerMoved(end)]).drop_without_applying_deltas();
        frame(
            &ctx,
            &mut repl,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
        .drop_without_applying_deltas();
        let output = frame(&ctx, &mut repl, vec![]);
        assert!(separator(&output) < initial_y - 50.0);
        output.drop_without_applying_deltas();
    }

    #[test]
    fn editor_shortcuts_follow_focus_and_keep_clipboard_behavior() {
        let ctx = egui::Context::default();
        let script_id = egui::Id::unique("test_script");
        let repl_id = egui::Id::unique("test_repl");
        let mut script = "(define (square x)".to_owned();
        let mut input = "(+ 1 2)".to_owned();
        let mut submitted = [false; 2];
        let mut draw = |events: Vec<egui::Event>, focus: egui::Id| {
            ctx.memory_mut(|m| m.request_focus(focus));
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 700.0),
                    )),
                    focused: true,
                    events,
                    ..Default::default()
                },
                |ui| {
                    submitted[0] = code_editor(ui, script_id, &mut script, 100.0, None);
                    submitted[1] = code_editor(ui, repl_id, &mut input, 100.0, None);
                },
            )
            .drop_without_applying_deltas();
            submitted
        };
        assert_eq!(draw(vec![], script_id), [false, false]);
        assert_eq!(draw(vec![ctrl(egui::Key::Enter)], script_id), [true, false]);
        assert_eq!(draw(vec![ctrl(egui::Key::Enter)], repl_id), [false, true]);
        draw(
            vec![ctrl(egui::Key::A), egui::Event::Paste("кириллица".into())],
            repl_id,
        );
        draw(vec![egui::Event::Copy], repl_id);
        assert_eq!(script, "(define (square x)");
        assert_eq!(input, "кириллица");
    }

    fn finish(repl: &mut Repl) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while repl.busy {
            repl.poll();
            assert!(std::time::Instant::now() < deadline, "Engine timed out");
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn script_and_repl_share_engine_but_keep_independent_buffers() {
        let (tx, rx) = mpsc::channel();
        let mut repl = Repl::new(move |engine| crate::register_plot_functions(engine, tx));
        let source = "(define (square x) (* x x))\n(plot/add! \"sample\" \"degC\")\n(plot/point! \"sample\" 0.0 20.0)";
        repl.script.text = source.into();
        repl.input = "(square 5)".into();
        repl.run_script();
        assert!(repl.busy);
        repl.execute_input(); // Busy requests must not queue, clear input or enter history.
        assert_eq!(repl.input, "(square 5)");
        assert!(repl.command_history.commands.is_empty());
        finish(&mut repl);
        repl.execute_input();
        assert!(repl.input.is_empty());
        finish(&mut repl);
        assert_eq!(repl.last_output, "25");
        assert_eq!(repl.script.text, source);
        repl.input = "(plot/point! \"sample\" 1.0 (square 5.0))".into();
        repl.execute_input();
        finish(&mut repl);
        let commands: Vec<_> = rx.try_iter().collect();
        assert_eq!(commands.len(), 3);
        assert!(matches!(&commands[0], crate::PlotCommand::Add { name, .. } if name == "sample"));
        assert!(matches!(&commands[2], crate::PlotCommand::Point { value, .. } if *value == 25.0));
        repl.input = "draft".into();
        assert!(repl.command_history.previous(&mut repl.input));
        assert!(repl.input.starts_with("(plot/point!"));
        assert!(repl.command_history.next(&mut repl.input));
        assert_eq!(repl.input, "draft");
        assert_eq!(repl.history.len(), 6);
        repl.script.text = "(unknown-function)".into();
        repl.run_script();
        finish(&mut repl);
        assert!(repl.last_output.starts_with("Error:"));
        assert_eq!(repl.script.text, "(unknown-function)");
        repl.input = "(square 6)".into();
        repl.execute_input();
        finish(&mut repl);
        assert_eq!(repl.last_output, "36");
    }

    #[test]
    fn file_execution_resolves_relative_modules_without_changing_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("helper.scm"),
            "(provide twice)\n(define (twice x) (* x 2))",
        )
        .unwrap();
        let path = dir.path().join("experiment.scm");
        std::fs::write(
            &path,
            "(require \"helper.scm\")\n(define answer (twice 21))\nanswer",
        )
        .unwrap();
        let mut repl = Repl::new(|_| {});
        repl.script.open(&path).unwrap();
        assert!(!repl.busy); // Opening is never evaluation.
        repl.run_script();
        finish(&mut repl);
        assert_eq!(repl.last_output, "42");
        repl.run_script();
        finish(&mut repl);
        assert_eq!(repl.last_output, "42");
        repl.script.text = repl.script.text.replace("twice 21", "twice 22");
        repl.run_script();
        finish(&mut repl);
        assert_eq!(repl.last_output, "44");
        assert!(repl.script.dirty());
        assert!(std::fs::read_to_string(&path).unwrap().contains("twice 21"));
        repl.input = "answer".into();
        repl.execute_input();
        finish(&mut repl);
        assert_eq!(repl.last_output, "44");
        assert_eq!(std::env::current_dir().unwrap(), cwd);
    }

    #[test]
    fn new_and_open_wait_for_confirmation_without_destroying_buffers() {
        let mut repl = Repl::new(|_| {});
        repl.script.text = "unsaved script".into();
        repl.input = "unfinished REPL".into();
        for action in [FileAction::New, FileAction::Open] {
            repl.request_file_action(action);
            assert!(repl.pending_action.is_some());
            repl.pending_action = None; // Cancel.
            assert_eq!(repl.script.text, "unsaved script");
            assert_eq!(repl.input, "unfinished REPL");
        }
        repl.perform_file_action(FileAction::New);
        assert!(repl.script.text.is_empty());
        assert_eq!(repl.input, "unfinished REPL");
    }

    #[test]
    fn failed_submission_preserves_repl_input_and_history() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut repl = Repl::new(|_| {});
        repl.request_tx = tx;
        repl.input = "(+ 1 2)".into();
        repl.execute_input();
        assert_eq!(repl.input, "(+ 1 2)");
        assert!(repl.command_history.commands.is_empty());
        assert!(!repl.busy);
        assert_eq!(repl.last_output, "Steel Engine unavailable");
    }

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
