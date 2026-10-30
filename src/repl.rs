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

use files::{SaveError, ScriptFile};

const MAX_COMMAND_HISTORY: usize = 200;
const MAX_HIDDEN_HISTORY: usize = 200;
const MAX_HISTORY_ENTRY_BYTES: usize = 16 * 1024;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileAction {
    New,
    Open,
    Close,
}

pub struct Repl {
    script: ScriptFile,
    input: String,
    history: VecDeque<String>,
    last_output: String,
    scroll_history_to_end: bool,
    file_error: Option<String>,
    script_format_message: Option<String>,
    repl_format_message: Option<String>,
    pending_action: Option<FileAction>,
    pending_save: Option<PathBuf>,
    close_allowed: bool,
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
                        if text.is_empty() { "ok".into() } else { text }
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
            history: VecDeque::new(),
            last_output: String::new(),
            scroll_history_to_end: false,
            file_error: None,
            script_format_message: None,
            repl_format_message: None,
            pending_action: None,
            pending_save: None,
            close_allowed: false,
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
                    self.record_history(&result);
                    self.last_output = result;
                    self.scroll_history_to_end = true;
                    self.busy = false;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.busy {
                        self.last_output = "Steel Engine unavailable".into();
                        self.record_history("Steel Engine unavailable");
                        self.scroll_history_to_end = true;
                        self.busy = false;
                    }
                    break;
                }
            }
        }
    }

    fn record_history(&mut self, entry: &str) {
        if self.history.len() == MAX_HIDDEN_HISTORY {
            self.history.pop_front();
        }
        let mut end = entry.len().min(MAX_HISTORY_ENTRY_BYTES);
        if end < entry.len() {
            end -= "…".len();
            while !entry.is_char_boundary(end) {
                end -= 1;
            }
            self.history.push_back(format!("{}…", &entry[..end]));
        } else {
            self.history.push_back(entry.to_owned());
        }
    }

    fn submit(&mut self, source: String, path: Option<PathBuf>, entry: &str) -> bool {
        if self.busy || source.trim().is_empty() {
            return false;
        }
        if self.request_tx.send(EvalRequest { source, path }).is_err() {
            self.last_output = "Steel Engine unavailable".into();
            self.record_history("Steel Engine unavailable");
            self.scroll_history_to_end = true;
            return false;
        }
        self.record_history(entry);
        self.busy = true;
        true
    }

    fn run_script(&mut self) {
        let name = self
            .script
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "несохранённый буфер".into());
        let entry = format!("Run script: {name}");
        self.submit(self.script.text.clone(), self.script.path.clone(), &entry);
    }

    fn execute_input(&mut self) {
        let source = self.input.clone();
        if self.submit(source.clone(), None, &format!("> {source}")) {
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
            FileAction::Close => self.close_allowed = true,
            FileAction::New => {
                self.script = ScriptFile::default();
                self.file_error = None;
                self.script_format_message = None;
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
                            self.script_format_message = None;
                            self.reset_script_cursor = true;
                        }
                        Err(error) => self.file_error = Some(format!("Open: {error}")),
                    }
                }
            }
        }
    }

    fn save(&mut self, save_as: bool, ctx: &egui::Context) -> bool {
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
        let saved = self.save_to_path(path, false, ctx);
        if saved {
            self.show_save_warning();
        }
        saved
    }

    fn save_to_path(&mut self, path: PathBuf, overwrite: bool, ctx: &egui::Context) -> bool {
        let (content, warning) = match schemat::format_string(&self.script.text) {
            Ok(content) => (content, None),
            Err(error) => (
                self.script.text.clone(),
                Some(format!(
                    "Сохранён исходный текст без форматирования: {error}"
                )),
            ),
        };
        let saved = if !overwrite && content == self.script.text {
            self.script.save_as(&path)
        } else {
            self.script.write_content(&path, overwrite, &content)
        };
        match saved {
            Ok(()) => {
                replace_editor_text(
                    ctx,
                    egui::Id::unique("steel_script_editor"),
                    &mut self.script.text,
                    content,
                );
                self.script_format_message = warning;
                self.file_error = None;
                self.pending_save = None;
                true
            }
            Err(SaveError::Conflict) => {
                self.pending_save = Some(path);
                false
            }
            Err(SaveError::Io(error)) => {
                self.file_error = Some(format!("Save: {error}"));
                self.pending_save = None;
                false
            }
        }
    }

    fn show_save_warning(&self) {
        if let Some(message) = &self.script_format_message {
            rfd::MessageDialog::new()
                .set_title("Steel: форматирование")
                .set_level(rfd::MessageLevel::Warning)
                .set_description(message)
                .show();
        }
    }

    fn complete_pending_action(&mut self) {
        if let Some(action) = self.pending_action.take() {
            self.perform_file_action(action);
        }
    }

    fn cancel_pending_action(&mut self) {
        self.pending_action = None;
        self.pending_save = None;
    }

    fn overwrite_ui(&mut self, ui: &mut egui::Ui) {
        let Some(path) = self.pending_save.clone() else {
            return;
        };
        ui.label(format!(
            "Файл изменён извне, удалён или выбран другой существующий файл: {}",
            path.display()
        ));
        ui.label("Перезаписать файл содержимым редактора?");
        ui.horizontal(|ui| {
            if ui.button("Overwrite").clicked() && self.save_to_path(path, true, ui.ctx()) {
                self.show_save_warning();
                self.complete_pending_action();
            }
            if ui.button("Cancel overwrite").clicked() {
                self.pending_save = None;
            }
        });
    }

    fn request_close(&mut self) -> bool {
        if self.close_allowed || !self.script.dirty() {
            return true;
        }
        self.pending_action = Some(FileAction::Close);
        false
    }

    pub fn close_ui(&mut self, ui: &mut egui::Ui) {
        if ui.input(|i| i.viewport().close_requested()) && !self.request_close() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.pending_action == Some(FileAction::Close) {
            egui::Modal::new(egui::Id::unique("unsaved_script_on_exit")).show(ui.ctx(), |ui| {
                ui.heading("Несохранённый скрипт");
                if let Some(error) = &self.file_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if self.pending_save.is_some() {
                    self.overwrite_ui(ui);
                } else {
                    ui.label("Сохранить изменения перед закрытием lab-plot?");
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() && self.save(false, ui.ctx()) {
                            self.complete_pending_action();
                        }
                        if ui.button("Discard").clicked() {
                            self.pending_save = None;
                            self.complete_pending_action();
                        }
                        if ui.button("Cancel").clicked() {
                            self.cancel_pending_action();
                        }
                    });
                }
            });
        }
        if self.close_allowed {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.pending_action.is_none() && self.pending_save.is_none() && ui.input(|i| i.focused) {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
                self.request_file_action(FileAction::New);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
                self.request_file_action(FileAction::Open);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
                self.save(false, ui.ctx());
            }
        }

        ui.add_enabled_ui(
            self.pending_action.is_none() && self.pending_save.is_none(),
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button("New").clicked() {
                        self.request_file_action(FileAction::New);
                    }
                    if ui.button("Open").clicked() {
                        self.request_file_action(FileAction::Open);
                    }
                    if ui.button("Save").clicked() {
                        self.save(false, ui.ctx());
                    }
                    if ui.button("Save As").clicked() {
                        self.save(true, ui.ctx());
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Run"))
                        .clicked()
                    {
                        self.run_script();
                    }
                });
            },
        );
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
        if let Some(message) = &self.script_format_message {
            ui.colored_label(ui.visuals().warn_fg_color, message);
        }
        if self.pending_action != Some(FileAction::Close) && self.pending_save.is_some() {
            self.overwrite_ui(ui);
        } else if let Some(action) = self.pending_action
            && action != FileAction::Close
        {
            ui.label("Несохранённые изменения. Сохранить перед продолжением?");
            ui.horizontal(|ui| {
                if ui.button("Save and continue").clicked() && self.save(false, ui.ctx()) {
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

        ui.add_enabled_ui(
            self.pending_action.is_none() && self.pending_save.is_none(),
            |ui| {
                let height = ui.available_height();
                egui::Panel::bottom("steel_repl_panel")
                    .resizable(true)
                    .default_size(height * 0.4)
                    .min_size(140.0)
                    .max_size((height - 140.0).max(140.0))
                    .show(ui, |ui| self.repl_ui(ui));
                egui::CentralPanel::default().show(ui, |ui| {
                    let id = egui::Id::unique("steel_script_editor");
                    if self.reset_script_cursor {
                        egui::text_edit::TextEditState::default().store(ui.ctx(), id);
                        self.reset_script_cursor = false;
                    }
                    let submit = egui::ScrollArea::vertical()
                        .id_salt("script_scroll")
                        .show(ui, |ui| {
                            code_editor(
                                ui,
                                id,
                                &mut self.script.text,
                                ui.available_height(),
                                None,
                                &mut self.script_format_message,
                            )
                        })
                        .inner;
                    if submit {
                        self.run_script();
                    }
                });
            },
        );
    }

    fn repl_ui(&mut self, ui: &mut egui::Ui) {
        ui.set_min_height(ui.available_height());
        let width = ui.available_width();
        egui::Panel::right("steel_repl_history")
            .resizable(true)
            .default_size(width * 0.4)
            .min_size(160.0)
            .max_size((width - 220.0).max(160.0))
            .show(ui, |ui| {
                ui.take_available_space();
                ui.strong("History");
                egui::ScrollArea::vertical()
                    .id_salt("execution_history_scroll")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (index, entry) in self.history.iter().enumerate() {
                            let response = ui.add(
                                egui::Label::new(egui::RichText::new(entry).monospace())
                                    .wrap()
                                    .selectable(true),
                            );
                            if self.scroll_history_to_end && index + 1 == self.history.len() {
                                response.scroll_to_me(Some(egui::Align::BOTTOM));
                            }
                        }
                    });
                self.scroll_history_to_end = false;
            });
        egui::CentralPanel::default().show(ui, |ui| self.repl_input_ui(ui));
    }

    fn repl_input_ui(&mut self, ui: &mut egui::Ui) {
        ui.set_min_height(ui.available_height());
        ui.strong("REPL");
        if let Some(message) = &self.repl_format_message {
            ui.colored_label(ui.visuals().warn_fg_color, message);
        }
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
        let input_height = (ui.available_height() - 60.0).max(40.0);
        let submit = egui::ScrollArea::vertical()
            .id_salt("repl_input_scroll")
            .max_height(input_height)
            .show(ui, |ui| {
                code_editor(
                    ui,
                    id,
                    &mut self.input,
                    input_height,
                    cursor,
                    &mut self.repl_format_message,
                )
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
        });
        ui.weak("Ctrl+Enter · Ctrl+PageUp/PageDown");
    }
}

// Keep selection direction, mapping by non-whitespace characters rather than byte offsets.
fn mapped_cursor(old: &str, new: &str, index: usize) -> usize {
    if index >= old.chars().count() {
        return new.chars().count();
    }
    let count = old
        .chars()
        .take(index)
        .filter(|ch| !ch.is_whitespace())
        .count();
    let in_space = old.chars().nth(index).is_some_and(char::is_whitespace);
    if count == 0 && in_space {
        return 0;
    }
    let mut seen = 0;
    for (position, ch) in new.chars().enumerate() {
        if !ch.is_whitespace() {
            if !in_space && seen == count {
                return position;
            }
            seen += 1;
            if in_space && seen == count {
                return position + 1;
            }
        }
    }
    new.chars().count()
}

fn replace_editor_text(ctx: &egui::Context, id: egui::Id, text: &mut String, formatted: String) {
    if *text == formatted {
        return;
    }
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let range = state.cursor.char_range().unwrap_or_else(|| {
        egui::text::CCursorRange::one(egui::text::CCursor::new(text.chars().count()))
    });
    let mut mapped = range;
    mapped.primary.index = mapped_cursor(text, &formatted, range.primary.index.0).into();
    mapped.secondary.index = mapped_cursor(text, &formatted, range.secondary.index.0).into();
    mapped.h_pos = None;
    let now = ctx.input(|i| i.time);
    let mut undoer = state.undoer();
    let before = (range, text.clone());
    undoer.feed_state(now, &before);
    undoer.add_undo(&before);
    let after = (mapped, formatted.clone());
    undoer.feed_state(now, &after);
    undoer.add_undo(&after);
    state.set_undoer(undoer);
    state.cursor.set_char_range(Some(mapped));
    state.store(ctx, id);
    *text = formatted;
}

/// Shared editing behavior, with distinct buffers, widget IDs and undo states.
fn code_editor(
    ui: &mut egui::Ui,
    id: egui::Id,
    text: &mut String,
    height: f32,
    cursor: Option<usize>,
    format_message: &mut Option<String>,
) -> bool {
    let focused = ui.is_enabled() && ui.memory(|m| m.has_focus(id)) && ui.input(|i| i.focused);
    let submit =
        focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter));
    if focused
        && ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::F)
        })
    {
        match schemat::format_string(text) {
            Ok(formatted) => {
                replace_editor_text(ui.ctx(), id, text, formatted);
                *format_message = None;
            }
            Err(error) => *format_message = Some(format!("Format: {error}")),
        }
    }
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
        let cursor = if focused {
            egui::TextEdit::load_state(ui.ctx(), id)
                .and_then(|state| state.cursor.char_range())
                .map(|range| range.primary.index.0)
        } else {
            None
        };
        let mut job = highlight::highlight_with_cursor(text.as_str(), cursor);
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

    #[test]
    fn formatting_messages_belong_to_their_editor_and_script_saves() {
        let ctx = egui::Context::default();
        let mut repl = Repl::new(|_| {});
        let script_id = egui::Id::unique("steel_script_editor");
        repl.script.text = "(define upper".into();
        repl.input = "(define lower".into();
        frame(&ctx, &mut repl, vec![]).drop_without_applying_deltas();
        ctx.memory_mut(|m| m.request_focus(script_id));
        frame(&ctx, &mut repl, vec![format_shortcut()]).drop_without_applying_deltas();
        let script_error = repl.script_format_message.clone().unwrap();
        assert!(repl.repl_format_message.is_none());

        // Focus the actual lower TextEdit, so the test covers the production UI wiring.
        let pos = egui::pos2(40.0, 520.0);
        for pressed in [true, false] {
            frame(
                &ctx,
                &mut repl,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .drop_without_applying_deltas();
        }
        let repl_id = ctx.memory(|m| m.focused()).unwrap();
        assert_ne!(repl_id, script_id);
        frame(&ctx, &mut repl, vec![format_shortcut()]).drop_without_applying_deltas();
        let repl_error = repl.repl_format_message.clone().unwrap();
        assert_eq!(
            repl.script_format_message.as_deref(),
            Some(script_error.as_str())
        );

        let output = frame(&ctx, &mut repl, vec![]);
        let message_y = |message: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::epaint::Shape::Text(text) = &shape.shape
                        && text.galley.job.text == message
                    {
                        Some(text.pos.y)
                    } else {
                        None
                    }
                })
                .unwrap()
        };
        assert!(message_y(&script_error) < message_y(&repl_error));
        output.drop_without_applying_deltas();

        repl.script.text = "( + 1 2 )".into();
        ctx.memory_mut(|m| m.request_focus(script_id));
        frame(&ctx, &mut repl, vec![format_shortcut()]).drop_without_applying_deltas();
        assert!(repl.script_format_message.is_none());
        assert_eq!(
            repl.repl_format_message.as_deref(),
            Some(repl_error.as_str())
        );

        let dir = tempfile::tempdir().unwrap();
        repl.script.text = "(define incomplete".into();
        assert!(repl.save_to_path(dir.path().join("incomplete.scm"), false, &ctx));
        let save_warning = repl.script_format_message.clone().unwrap();
        assert!(save_warning.starts_with("Сохранён исходный текст"));
        assert_eq!(
            repl.repl_format_message.as_deref(),
            Some(repl_error.as_str())
        );

        repl.input = "( + 3 4 )".into();
        ctx.memory_mut(|m| m.request_focus(repl_id));
        frame(&ctx, &mut repl, vec![format_shortcut()]).drop_without_applying_deltas();
        assert!(repl.repl_format_message.is_none());
        assert_eq!(
            repl.script_format_message.as_deref(),
            Some(save_warning.as_str())
        );
        assert_eq!(repl.input, "(+ 3 4)\n");
        assert!(!repl.busy);
    }

    fn format_shortcut() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::F,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                command: true,
                shift: true,
                ..Default::default()
            },
        }
    }

    fn edit_frame(
        ctx: &egui::Context,
        id: egui::Id,
        text: &mut String,
        events: Vec<egui::Event>,
        message: &mut Option<String>,
    ) {
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.run_ui(
            egui::RawInput {
                focused: true,
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 700.0),
                )),
                ..Default::default()
            },
            |ui| {
                assert!(!code_editor(ui, id, text, 100.0, None, message));
            },
        )
        .drop_without_applying_deltas();
    }

    #[test]
    fn formatting_is_one_undo_step_and_keeps_earlier_undo() {
        let ctx = egui::Context::default();
        let id = egui::Id::unique("format_undo");
        let original = "(define x 1)\n";
        let edited = "(  define   x  2 )";
        let mut text = original.to_owned();
        let mut message = None;
        edit_frame(&ctx, id, &mut text, vec![], &mut message);
        edit_frame(
            &ctx,
            id,
            &mut text,
            vec![ctrl(egui::Key::A), egui::Event::Paste(edited.into())],
            &mut message,
        );
        assert_eq!(text, edited);
        edit_frame(&ctx, id, &mut text, vec![format_shortcut()], &mut message);
        assert_eq!(text, "(define x 2)\n");
        assert!(message.is_none());
        edit_frame(&ctx, id, &mut text, vec![format_shortcut()], &mut message);
        edit_frame(&ctx, id, &mut text, vec![ctrl(egui::Key::Z)], &mut message);
        assert_eq!(text, edited);
        edit_frame(&ctx, id, &mut text, vec![ctrl(egui::Key::Z)], &mut message);
        assert_eq!(text, original);
    }

    #[test]
    fn formatter_errors_keep_text_and_editor_cursors_are_independent() {
        let ctx = egui::Context::default();
        let script_id = egui::Id::unique("format_script");
        let input_id = egui::Id::unique("format_input");
        let mut script = "( define  имя   10 )".to_owned();
        let mut input = "(define (unfinished".to_owned();
        let mut message = None;
        edit_frame(&ctx, script_id, &mut script, vec![], &mut message);
        let mut state = egui::TextEdit::load_state(&ctx, script_id).unwrap();
        // Select the Cyrillic identifier, preserving selection direction after whitespace changes.
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(10),
                egui::text::CCursor::new(13),
            )));
        state.store(&ctx, script_id);
        edit_frame(
            &ctx,
            script_id,
            &mut script,
            vec![format_shortcut()],
            &mut message,
        );
        let range = egui::TextEdit::load_state(&ctx, script_id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        let selected: String = script
            .chars()
            .skip(range.as_sorted_char_range().start.0)
            .take(range.as_sorted_char_range().end.0 - range.as_sorted_char_range().start.0)
            .collect();
        assert_eq!(selected, "имя");
        let prior = range;
        edit_frame(&ctx, input_id, &mut input, vec![], &mut message);
        edit_frame(
            &ctx,
            input_id,
            &mut input,
            vec![format_shortcut()],
            &mut message,
        );
        assert!(message.is_some());
        assert_eq!(input, "(define (unfinished");
        assert_eq!(
            egui::TextEdit::load_state(&ctx, script_id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap(),
            prior
        );
        input = "( +  1  2 )".into();
        edit_frame(
            &ctx,
            input_id,
            &mut input,
            vec![format_shortcut()],
            &mut message,
        );
        assert_eq!(input, "(+ 1 2)\n");
        assert_eq!(script, "(define имя 10)\n");
    }

    #[test]
    fn save_formats_only_after_success_and_undo_restores_dirty_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("format-save.scm");
        let ctx = egui::Context::default();
        let mut repl = Repl::new(|_| {});
        let raw = "(  define  имя   1 )";
        repl.script.text = raw.into();
        repl.script.save_as(&path).unwrap();
        frame(&ctx, &mut repl, vec![]).drop_without_applying_deltas();
        assert!(repl.save_to_path(path.clone(), false, &ctx));
        assert_eq!(repl.script.text, "(define имя 1)\n");
        assert!(!repl.script.dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repl.script.text);
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("steel_script_editor")));
        frame(&ctx, &mut repl, vec![ctrl(egui::Key::Z)]).drop_without_applying_deltas();
        assert_eq!(repl.script.text, raw);
        assert!(repl.script.dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "(define имя 1)\n");
        repl.script.text = "(define unfinished".into();
        assert!(repl.save_to_path(path.clone(), false, &ctx));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "(define unfinished");
        assert_eq!(repl.script.text, "(define unfinished");
        assert!(repl.script_format_message.is_some());
        assert!(!repl.script.dirty());
        assert!(!repl.busy);
        assert!(repl.history.is_empty());
    }

    #[test]
    fn conflicting_or_failed_save_never_applies_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflict.scm");
        let ctx = egui::Context::default();
        let mut repl = Repl::new(|_| {});
        repl.script.save_as(&path).unwrap();
        let raw = "( +  1   2 )";
        repl.script.text = raw.into();
        std::fs::write(&path, "; external change").unwrap();
        assert!(!repl.save_to_path(path.clone(), false, &ctx));
        repl.pending_save = None; // Cancel overwrite.
        assert_eq!(repl.script.text, raw);
        assert!(repl.script.dirty());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "; external change");
        assert!(!repl.save_to_path(dir.path().join("missing/file.scm"), false, &ctx));
        assert_eq!(repl.script.text, raw);
        assert!(repl.script.dirty());
    }

    #[test]
    fn void_success_values_and_errors_are_recorded_in_existing_history() {
        let mut repl = Repl::new(|_| {});
        repl.input = "(define x 10)".into();
        repl.execute_input();
        finish(&mut repl);
        assert_eq!(repl.last_output, "ok");
        assert_eq!(repl.history.back().unwrap(), "ok");
        assert!(repl.scroll_history_to_end);
        repl.input = "(+ x 5)".into();
        repl.execute_input();
        finish(&mut repl);
        assert_eq!(repl.last_output, "15");
        repl.input = "(unknown-function)".into();
        repl.execute_input();
        finish(&mut repl);
        assert!(repl.last_output.starts_with("Error:"));
        assert_eq!(repl.history.len(), 6);
        assert_eq!(repl.history[0], "> (define x 10)");
        assert_eq!(repl.history[2], "> (+ x 5)");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("definitions.scm");
        std::fs::write(&path, "(define y 20)").unwrap();
        repl.script.open(&path).unwrap();
        repl.run_script();
        finish(&mut repl);
        assert_eq!(repl.last_output, "ok");
        assert_eq!(repl.history[6], "Run script: definitions.scm");
        assert_eq!(repl.history[7], "ok");
        assert_eq!(repl.script.text, "(define y 20)");
        repl.input = "(+ x y)".into();
        repl.execute_input();
        finish(&mut repl);
        assert_eq!(repl.last_output, "30");
    }

    #[test]
    fn dirty_close_is_cancelled_at_viewport_level_and_cancel_keeps_buffer() {
        let mut repl = Repl::new(|_| {});
        assert!(repl.request_close());
        repl.script.text = "unsaved script".into();
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run_ui(raw, |ui| repl.close_ui(ui));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert!(!repl.close_allowed);
        assert!(repl.pending_action == Some(FileAction::Close));
        repl.cancel_pending_action();
        assert!(repl.pending_action.is_none());
        assert!(!repl.close_allowed);
        assert_eq!(repl.script.text, "unsaved script");
        output.drop_without_applying_deltas();
    }

    #[test]
    fn close_only_proceeds_after_successful_save_or_explicit_discard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("close.scm");
        let mut repl = Repl::new(|_| {});
        repl.script.save_as(&path).unwrap();
        repl.script.text = "unsaved".into();
        // Reading a directory as a file fails; it must never count as unchanged.
        repl.script.path = Some(dir.path().to_owned());
        assert!(!repl.request_close());
        assert!(!repl.save(false, &egui::Context::default()));
        assert!(repl.file_error.is_some());
        assert!(!repl.close_allowed);
        assert_eq!(repl.script.text, "unsaved");
        assert!(repl.script.dirty());
        repl.script.path = Some(path.clone());
        assert!(repl.save(false, &egui::Context::default()));
        repl.complete_pending_action();
        assert!(repl.close_allowed);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            schemat::format_string("unsaved").unwrap()
        );

        let mut discarded = Repl::new(|_| {});
        discarded.script.text = "discard me".into();
        assert!(!discarded.request_close());
        discarded.complete_pending_action(); // Discard button.
        assert!(discarded.close_allowed);
    }

    #[test]
    fn conflicting_save_keeps_buffer_until_overwrite_is_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflict.scm");
        std::fs::write(&path, "original").unwrap();
        let mut repl = Repl::new(|_| {});
        repl.script.open(&path).unwrap();
        repl.script.text = "local edit".into();
        std::fs::write(&path, "external edit").unwrap();
        assert!(!repl.save(false, &egui::Context::default()));
        assert!(repl.pending_save.is_some());
        repl.pending_save = None; // Cancel overwrite.
        assert_eq!(repl.script.text, "local edit");
        assert!(repl.script.dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");

        assert!(!repl.request_close());
        assert!(!repl.save(false, &egui::Context::default()));
        assert!(!repl.close_allowed);
        let target = repl.pending_save.clone().unwrap();
        assert!(repl.save_to_path(target, true, &egui::Context::default()));
        repl.complete_pending_action();
        assert!(repl.close_allowed);
        assert!(!repl.script.dirty());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            schemat::format_string("local edit").unwrap()
        );
    }

    #[test]
    fn hidden_history_has_bounded_utf8_entries_and_script_summaries() {
        let mut repl = Repl::new(|_| {});
        repl.script.text = format!(";{}", "large script source".repeat(10000));
        repl.run_script();
        finish(&mut repl);
        assert!(
            repl.history
                .front()
                .unwrap()
                .contains("несохранённый буфер")
        );
        assert!(
            !repl
                .history
                .front()
                .unwrap()
                .contains("large script source")
        );
        let long_result = "🌡температура".repeat(MAX_HISTORY_ENTRY_BYTES);
        let (tx, rx) = mpsc::channel();
        repl.response_rx = rx;
        for _ in 0..MAX_HIDDEN_HISTORY + 5 {
            tx.send(long_result.clone()).unwrap();
            repl.poll();
        }
        assert_eq!(repl.history.len(), MAX_HIDDEN_HISTORY);
        assert!(
            repl.history
                .iter()
                .all(|entry| entry.len() <= MAX_HISTORY_ENTRY_BYTES && entry.ends_with('…'))
        );
        assert_eq!(repl.last_output, long_result);
        assert!(repl.command_history.commands.is_empty());
    }

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
        repl.script.text = "; несохранённый скрипт\n".into();
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
    fn ui_renders_execution_history_and_preserves_state_when_reopened() {
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
        repl.history.push_back("PREVIOUS_RESULT_VISIBLE".into());
        repl.last_output = "LATEST_RESULT_VISIBLE".into();
        repl.history.push_back(repl.last_output.clone());
        let ctx = egui::Context::default();
        for _ in 0..2 {
            // Hide/reopen keeps this same Repl, exactly as LabPlot does.
            let output = frame(&ctx, &mut repl, vec![]);
            let mut text = String::new();
            for shape in &output.shapes {
                visible_text(&shape.shape, &mut text);
            }
            assert!(text.contains("LATEST_RESULT_VISIBLE"));
            assert!(text.contains("PREVIOUS_RESULT_VISIBLE"));
            assert_eq!(repl.script.text, "(define retained 42)");
            assert_eq!(repl.input, "unfinished input");
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn repl_panels_keep_proportions_and_both_separators_can_be_dragged() {
        fn separator(output: &egui::FullOutput, vertical: bool) -> egui::Pos2 {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::epaint::Shape::LineSegment { points, .. } = &shape.shape {
                        let delta = points[1] - points[0];
                        if (vertical && delta.x == 0.0 && delta.y.abs() > 100.0)
                            || (!vertical && delta.y == 0.0 && delta.x.abs() > 600.0)
                        {
                            return Some(points[0]);
                        }
                    }
                    None
                })
                .expect("panel separator")
        }
        fn drag(ctx: &egui::Context, repl: &mut Repl, start: egui::Pos2, end: egui::Pos2) {
            for events in [
                vec![egui::Event::PointerMoved(start)],
                vec![egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }],
                vec![egui::Event::PointerMoved(end)],
                vec![egui::Event::PointerButton {
                    pos: end,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ] {
                frame(ctx, repl, events).drop_without_applying_deltas();
            }
        }
        let ctx = egui::Context::default();
        let mut repl = Repl::new(|_| {});
        let output = frame(&ctx, &mut repl, vec![]);
        let horizontal_y = separator(&output, false).y;
        let vertical_x = separator(&output, true).x;
        assert!((400.0..490.0).contains(&horizontal_y), "40% REPL height");
        assert!(
            (460.0..510.0).contains(&vertical_x),
            "60/40 input/history width"
        );
        output.drop_without_applying_deltas();

        drag(
            &ctx,
            &mut repl,
            egui::pos2(400.0, horizontal_y),
            egui::pos2(400.0, horizontal_y - 80.0),
        );
        let output = frame(&ctx, &mut repl, vec![]);
        assert!(separator(&output, false).y < horizontal_y - 50.0);
        output.drop_without_applying_deltas();

        drag(
            &ctx,
            &mut repl,
            egui::pos2(vertical_x, 600.0),
            egui::pos2(vertical_x - 80.0, 600.0),
        );
        let output = frame(&ctx, &mut repl, vec![]);
        let moved_x = separator(&output, true).x;
        assert!(moved_x < vertical_x - 50.0);
        let moved_y = separator(&output, false).y;
        output.drop_without_applying_deltas();

        // Drag beyond the limits: neither script editor nor REPL input may disappear.
        drag(
            &ctx,
            &mut repl,
            egui::pos2(moved_x, 600.0),
            egui::pos2(-1000.0, 600.0),
        );
        drag(
            &ctx,
            &mut repl,
            egui::pos2(400.0, moved_y),
            egui::pos2(400.0, -1000.0),
        );
        let output = frame(&ctx, &mut repl, vec![]);
        assert!(separator(&output, true).x >= 220.0);
        assert!(separator(&output, false).y >= 140.0);
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
                    submitted[0] = code_editor(ui, script_id, &mut script, 100.0, None, &mut None);
                    submitted[1] = code_editor(ui, repl_id, &mut input, 100.0, None, &mut None);
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
