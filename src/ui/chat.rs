use crate::domain::{MessageContent, Role};
use crate::state::AppState;
use agent_client_protocol::{
    AvailableCommand, Cost, SessionModeState, SessionModelState, UsageUpdate,
};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState, RopeExt};
use gpui_component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_component::skeleton::Skeleton;
use gpui_component::text::TextView;
use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
use std::collections::HashSet;
use std::io::Read;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use uuid::Uuid;

mod support;
use support::render_config_option_row;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SuggestionKind {
    Slash,
    File,
}

#[derive(Clone)]
struct SuggestionItem {
    display: String,
    replacement: String,
}

#[derive(Clone)]
struct PickerOption {
    value: String,
    label: String,
}

impl PickerOption {
    fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

impl SelectItem for PickerOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

struct SuggestionState {
    kind: SuggestionKind,
    start: usize,
    items: Vec<SuggestionItem>,
    selected: usize,
}

struct TerminalReader {
    rx: std_mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    offset: usize,
}

impl TerminalReader {
    fn new(rx: std_mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            chunk: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for TerminalReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.offset >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) if !chunk.is_empty() => {
                    self.chunk = chunk;
                    self.offset = 0;
                }
                Ok(_) => {}
                Err(_) => return Ok(0),
            }
        }
        let remaining = &self.chunk[self.offset..];
        let len = remaining.len().min(buf.len());
        buf[..len].copy_from_slice(&remaining[..len]);
        self.offset += len;
        Ok(len)
    }
}

struct TerminalWidgetState {
    terminal: Entity<TerminalView>,
    tx: std_mpsc::Sender<Vec<u8>>,
    transcript: String,
}

const COLLAPSE_LINE_LIMIT: usize = 10;

pub struct ChatView {
    app_state: Entity<AppState>,
    list_state: ListState,
    listed_thread_id: Option<Uuid>,
    listed_count: usize,
    suggestion_scroll_handle: ScrollHandle,
    input_state: Entity<InputState>,
    mode_select_state: Entity<SelectState<Vec<PickerOption>>>,
    model_select_state: Entity<SelectState<Vec<PickerOption>>>,
    locked_to_bottom: bool,
    render_pending: bool,
    // Set by the list scroll_handler (fired while list_state is borrow_mut'd)
    // so we cannot call update_scroll_lock() directly there; render() processes it.
    user_scrolled: bool,
    suggestion_anchor: Option<(SuggestionKind, usize)>,
    suggestion_selected: usize,
    dismissed_suggestion: Option<(SuggestionKind, usize)>,
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    expanded_messages: HashSet<Uuid>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 10)
                .placeholder("Type and press Enter to send, Shift+Enter for new line...")
        });
        let mode_select_state =
            cx.new(|cx| SelectState::new(Vec::<PickerOption>::new(), None, window, cx));
        let model_select_state =
            cx.new(|cx| SelectState::new(Vec::<PickerOption>::new(), None, window, cx));

        cx.observe(&app_state, |this, _, cx| {
            if this.locked_to_bottom {
                this.scroll_to_bottom();
            }
            // Coalesce rapid state updates (e.g. streaming chunks) into at most
            // one re-render per ~16 ms so we stay near 60 fps.
            if !this.render_pending {
                this.render_pending = true;
                let background = cx.background_executor().clone();
                cx.spawn(
                    |this: gpui::WeakEntity<ChatView>, cx: &mut gpui::AsyncApp| {
                        let mut cx = cx.clone();
                        async move {
                            background.timer(Duration::from_millis(16)).await;
                            this.update(
                                &mut cx,
                                |this: &mut ChatView, cx: &mut Context<ChatView>| {
                                    this.render_pending = false;
                                    cx.notify();
                                },
                            )
                            .ok();
                        }
                    },
                )
                .detach();
            }
        })
        .detach();

        cx.subscribe(
            &input_state,
            |this, _input_state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.history_cursor = None;
                    this.reconcile_suggestion_visibility(cx);

                    let val = this.input_value(cx);
                    if let Some(thread_id) = this.app_state.read(cx).active_thread_id {
                        this.app_state.update(cx, |state, _| {
                            state.update_thread_draft(thread_id, val);
                        });
                    }

                    cx.notify();
                }
            },
        )
        .detach();

        cx.subscribe(
            &mode_select_state,
            |this, _state, event: &SelectEvent<Vec<PickerOption>>, cx| {
                let SelectEvent::Confirm(Some(mode_id)) = event else {
                    return;
                };
                if let Some(thread_id) = this.app_state.read(cx).active_thread_id {
                    this.app_state.update(cx, |state, cx| {
                        state.set_session_mode(cx, thread_id, mode_id.clone());
                    });
                }
            },
        )
        .detach();

        cx.subscribe(
            &model_select_state,
            |this, _state, event: &SelectEvent<Vec<PickerOption>>, cx| {
                let SelectEvent::Confirm(Some(model_id)) = event else {
                    return;
                };
                if let Some(thread_id) = this.app_state.read(cx).active_thread_id {
                    this.app_state.update(cx, |state, cx| {
                        state.set_session_model(cx, thread_id, model_id.clone());
                    });
                }
            },
        )
        .detach();

        cx.observe_keystrokes(|this, event, window, cx| {
            let key = event.keystroke.key.as_str();

            if let Some(suggestions) = this.compute_suggestions(cx) {
                if key == "escape" {
                    this.dismissed_suggestion = Some((suggestions.kind, suggestions.start));
                    cx.notify();
                    return;
                }
                if key == "up" || (event.keystroke.modifiers.control && event.keystroke.key == "k")
                {
                    this.select_previous_suggestion(suggestions.items.len());
                    cx.notify();
                    return;
                }
                if key == "down"
                    || (event.keystroke.modifiers.control && event.keystroke.key == "j")
                {
                    this.select_next_suggestion(suggestions.items.len());
                    cx.notify();
                    return;
                }
                if key == "tab" || (event.keystroke.modifiers.control && event.keystroke.key == "y")
                {
                    this.apply_suggestion(&suggestions, window, cx);
                    cx.notify();
                    return;
                }
            }

            if key == "up" && this.input_value(cx).trim().is_empty() {
                this.history_up(window, cx);
                return;
            }
            if key == "down"
                && (this.history_cursor.is_some() || this.input_value(cx).trim().is_empty())
            {
                this.history_down(window, cx);
                return;
            }
            if key == "enter" {
                if event.keystroke.modifiers.shift {
                    this.input_state.update(cx, |state, cx| {
                        state.insert("\n", window, cx);
                    });
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if !event.keystroke.modifiers.secondary() {
                    this.submit_input(window, cx);
                }
            }
        })
        .detach();

        let list_state = {
            let this = cx.entity();
            let state = ListState::new(0, ListAlignment::Top, px(512.0));
            let weak = this.downgrade();
            state.set_scroll_handler(move |_event, _window, app| {
                // list_state.0 is borrow_mut'd when this handler fires, so we
                // must NOT call borrow() on it (e.g. via max_offset_for_scrollbar).
                // Set a flag; render() will process it safely.
                let _ = weak.update(app, |this, _cx| {
                    this.user_scrolled = true;
                });
            });
            state
        };

        Self {
            app_state,
            list_state,
            listed_thread_id: None,
            listed_count: 0,
            suggestion_scroll_handle: ScrollHandle::new(),
            input_state,
            mode_select_state,
            model_select_state,
            locked_to_bottom: true,
            render_pending: false,
            user_scrolled: false,
            suggestion_anchor: None,
            suggestion_selected: 0,
            dismissed_suggestion: None,
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            expanded_messages: HashSet::new(),
        }
    }

    fn set_input_value(
        &self,
        value: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = value.into();
        self.input_state.update(cx, |state, cx| {
            state.set_value(value, window, cx);
            state.focus(window, cx);
        });
    }

    fn input_value(&self, cx: &Context<Self>) -> String {
        self.input_state.read(cx).value().to_string()
    }

    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.input_value(cx);
        let content = raw.trim().to_owned();
        if content.is_empty() {
            return;
        }

        let thread_id = self.app_state.read(cx).active_thread_id;
        if let Some(thread_id) = thread_id {
            self.app_state.update(cx, |state, cx| {
                state.send_user_message(cx, thread_id, &content);
            });
            if self
                .input_history
                .last()
                .is_none_or(|last| last != &content)
            {
                self.input_history.push(content);
            }
            self.history_cursor = None;
            self.history_draft.clear();
            self.set_input_value("", window, cx);
            self.locked_to_bottom = true;
            cx.notify();
        }
    }

    fn history_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_history.is_empty() {
            return;
        }
        let next_index = match self.history_cursor {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.input_value(cx);
                self.input_history.len().saturating_sub(1)
            }
        };
        self.history_cursor = Some(next_index);
        self.set_input_value(self.input_history[next_index].clone(), window, cx);
    }

    fn history_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.input_history.len() {
            let next_index = index + 1;
            self.history_cursor = Some(next_index);
            self.set_input_value(self.input_history[next_index].clone(), window, cx);
            return;
        }
        self.history_cursor = None;
        self.set_input_value(self.history_draft.clone(), window, cx);
    }

    fn update_scroll_lock(&mut self, cx: &mut Context<Self>) {
        // scroll_px_offset_for_scrollbar returns negative Y (content-moved-up convention),
        // so "at bottom" is when max_y + y ≈ 0.
        let offset = self.list_state.scroll_px_offset_for_scrollbar();
        let max_y = self.list_state.max_offset_for_scrollbar().height;
        self.locked_to_bottom = (max_y + offset.y).abs() <= px(2.0);

        if let Some(thread_id) = self.app_state.read(cx).active_thread_id {
            let locked = self.locked_to_bottom;
            self.app_state.update(cx, |state, _| {
                state.update_thread_scroll_state(thread_id, offset, locked);
            });
        }
    }

    fn scroll_to_bottom(&self) {
        let max = self.list_state.max_offset_for_scrollbar();
        self.list_state
            .set_offset_from_scrollbar(point(px(0.0), max.height));
    }

    fn suggestion_anchor_from_input(&self, input: &str) -> Option<(SuggestionKind, usize, String)> {
        if input.starts_with('/') && !input.contains(char::is_whitespace) {
            return Some((SuggestionKind::Slash, 0, input[1..].to_string()));
        }

        let token_start = input
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        let token = &input[token_start..];
        token
            .strip_prefix('@')
            .map(|query| (SuggestionKind::File, token_start, query.to_string()))
    }

    fn reconcile_suggestion_visibility(&mut self, cx: &Context<Self>) {
        let input = self.input_value(cx);
        if self.dismissed_suggestion.is_some()
            && self
                .suggestion_anchor_from_input(&input)
                .map(|(kind, start, _)| (kind, start))
                != self.dismissed_suggestion
        {
            self.dismissed_suggestion = None;
        }
    }

    fn compute_suggestions(&mut self, cx: &Context<Self>) -> Option<SuggestionState> {
        let input = self.input_value(cx);
        self.reconcile_suggestion_visibility(cx);
        let (kind, start, query) = self.suggestion_anchor_from_input(&input)?;
        if self.dismissed_suggestion == Some((kind, start)) {
            return None;
        }

        let items = match kind {
            SuggestionKind::Slash => {
                let commands = self
                    .app_state
                    .read(cx)
                    .active_thread_available_commands()
                    .unwrap_or_default();
                slash_suggestion_items(&commands, &query)
            }
            SuggestionKind::File => {
                let thread_id = self.app_state.read(cx).active_thread_id?;
                let files = self
                    .app_state
                    .read(cx)
                    .workspace_relative_files_for_thread(thread_id, 1024);
                file_suggestion_items(&files, &query)
            }
        };
        if items.is_empty() {
            self.suggestion_anchor = None;
            return None;
        }

        let anchor = (kind, start);
        if self.suggestion_anchor != Some(anchor) {
            self.suggestion_anchor = Some(anchor);
            self.suggestion_selected = items.len().saturating_sub(1);
        } else if self.suggestion_selected >= items.len() {
            self.suggestion_selected = items.len().saturating_sub(1);
        }

        Some(SuggestionState {
            kind,
            start,
            selected: self.suggestion_selected,
            items,
        })
    }

    fn select_previous_suggestion(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.suggestion_selected = if self.suggestion_selected == 0 {
            total - 1
        } else {
            self.suggestion_selected - 1
        };
    }

    fn select_next_suggestion(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.suggestion_selected = (self.suggestion_selected + 1) % total;
    }

    fn apply_suggestion(
        &mut self,
        state: &SuggestionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = state.items.get(state.selected) else {
            return;
        };
        let mut input = self.input_value(cx);
        input.replace_range(state.start..input.len(), &item.replacement);
        self.suggestion_anchor = None;
        self.dismissed_suggestion = None;
        self.set_input_value(input, window, cx);
    }

    fn render_readonly_code(
        message_id: Uuid,
        language: &str,
        content: String,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let language = SharedString::from(language.to_string());
        let input_state = window
            .use_keyed_state(
                SharedString::from(format!("chat-diff-input-{}", message_id)),
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .multi_line(true)
                            .code_editor(language)
                            .line_number(true)
                            .default_value(content.clone())
                    })
                },
            )
            .read(cx)
            .clone();

        input_state.update(cx, |state, cx| {
            if state.value().as_ref() != content {
                state.set_value(content, window, cx);
            }
        });

        div()
            .w_full()
            .h(px(220.0))
            .debug_selector(|| "chat-diff-input".to_string())
            .child(
                Input::new(&input_state)
                    .disabled(true)
                    .appearance(false)
                    .h_full(),
            )
            .into_any_element()
    }

    fn render_message_content(
        app_state: &Entity<AppState>,
        thread_id: Uuid,
        message_id: Uuid,
        content: &MessageContent,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        match content {
            MessageContent::Text(text) => {
                if text.contains("--- before\n+++ after") {
                    return Self::render_readonly_code(
                        message_id,
                        "diff",
                        text.clone(),
                        window,
                        cx,
                    );
                }

                TextView::markdown(
                    SharedString::from(format!("chat-md-{}", message_id)),
                    SharedString::from(text.clone()),
                    window,
                    cx,
                )
                .selectable(true)
                .into_any_element()
            }
            MessageContent::ToolCall(tool_call) => {
                let mut lines = vec![
                    format!("Tool: {}", tool_call.title),
                    format!(
                        "Kind: {}",
                        match tool_call.kind {
                            agent_client_protocol::ToolKind::Read => "read",
                            agent_client_protocol::ToolKind::Edit => "edit",
                            agent_client_protocol::ToolKind::Delete => "delete",
                            agent_client_protocol::ToolKind::Move => "move",
                            agent_client_protocol::ToolKind::Search => "search",
                            agent_client_protocol::ToolKind::Execute => "execute",
                            agent_client_protocol::ToolKind::Think => "think",
                            agent_client_protocol::ToolKind::Fetch => "fetch",
                            agent_client_protocol::ToolKind::SwitchMode => "switch_mode",
                            _ => "other",
                        }
                    ),
                    format!(
                        "Status: {}",
                        match tool_call.status {
                            agent_client_protocol::ToolCallStatus::Pending => "pending",
                            agent_client_protocol::ToolCallStatus::InProgress => "in_progress",
                            agent_client_protocol::ToolCallStatus::Completed => "completed",
                            agent_client_protocol::ToolCallStatus::Failed => "failed",
                            _ => "unknown",
                        }
                    ),
                ];

                let mut diff_block = None;
                let mut other_content = Vec::new();
                let mut terminal_ids = Vec::new();

                for content in &tool_call.content {
                    match content {
                        agent_client_protocol::ToolCallContent::Content(c) => {
                            if let agent_client_protocol::ContentBlock::Text(t) = &c.content {
                                other_content.push(t.text.clone());
                            }
                        }
                        agent_client_protocol::ToolCallContent::Diff(d) => {
                            lines.push(format!("Diff: {}", d.path.display()));
                            diff_block = Some(crate::state::render_diff_text(
                                d.old_text.as_deref(),
                                &d.new_text,
                            ));
                        }
                        agent_client_protocol::ToolCallContent::Terminal(terminal) => {
                            terminal_ids.push(terminal.terminal_id.to_string());
                        }
                        _ => {}
                    }
                }

                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        TextView::markdown(
                            SharedString::from(format!("chat-tool-meta-{}", message_id)),
                            SharedString::from(lines.join("\n")),
                            window,
                            cx,
                        )
                        .selectable(true),
                    )
                    .when_some(diff_block, |this, diff| {
                        this.child(Self::render_readonly_code(
                            message_id, "diff", diff, window, cx,
                        ))
                    })
                    .children(terminal_ids.into_iter().map(|terminal_id| {
                        let transcript = app_state
                            .read(cx)
                            .terminal_transcript_for_thread(thread_id, &terminal_id)
                            .unwrap_or_default();
                        Self::render_terminal_widget(
                            message_id,
                            terminal_id,
                            transcript,
                            window,
                            cx,
                        )
                    }))
                    .children(other_content.into_iter().enumerate().map(|(i, text)| {
                        TextView::markdown(
                            SharedString::from(format!("chat-tool-extra-{}-{}", message_id, i)),
                            SharedString::from(text),
                            window,
                            cx,
                        )
                        .selectable(true)
                    }))
                    .into_any_element()
            }
        }
    }

    fn terminal_config() -> TerminalConfig {
        TerminalConfig {
            cols: 120,
            rows: 12,
            font_family: "monospace".to_string(),
            font_size: px(12.0),
            scrollback: 10_000,
            line_height_multiplier: 1.2,
            padding: Edges::all(px(8.0)),
            colors: ColorPalette::default(),
        }
    }

    fn render_terminal_widget(
        message_id: Uuid,
        terminal_id: String,
        transcript: String,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let terminal_state = window.use_keyed_state(
            SharedString::from(format!("chat-terminal-{}-{}", message_id, terminal_id)),
            cx,
            |_window, cx| {
                let (tx, rx) = std_mpsc::channel::<Vec<u8>>();
                let terminal = cx.new(|cx| {
                    TerminalView::new(
                        std::io::sink(),
                        TerminalReader::new(rx),
                        Self::terminal_config(),
                        cx,
                    )
                });
                TerminalWidgetState {
                    terminal,
                    tx,
                    transcript: String::new(),
                }
            },
        );
        terminal_state.update(cx, move |state, _| {
            if state.transcript == transcript {
                return;
            }
            if let Some(delta) = transcript.strip_prefix(&state.transcript) {
                if !delta.is_empty() {
                    let _ = state.tx.send(delta.as_bytes().to_vec());
                }
            } else {
                let _ = state.tx.send(b"\x1bc".to_vec());
                if !transcript.is_empty() {
                    let _ = state.tx.send(transcript.as_bytes().to_vec());
                }
            }
            state.transcript = transcript;
        });
        let terminal = terminal_state.read(cx).terminal.clone();
        div()
            .w_full()
            .h(px(220.0))
            .rounded_md()
            .overflow_hidden()
            .child(terminal)
            .into_any_element()
    }

    /// Renders a single message row. Called from the list() render callback so it
    /// takes `Entity<AppState>` and `Entity<ChatView>` rather than `&mut self`.
    fn render_message_row(
        index: usize,
        app_state: &Entity<AppState>,
        this: &Entity<ChatView>,
        expanded_messages: &HashSet<Uuid>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let (
            bg,
            text_color,
            message_content,
            copy_content,
            thread_id,
            message_id,
            is_collapsible,
            is_expanded,
        ): (Rgba, Rgba, MessageContent, String, Uuid, Uuid, bool, bool) = {
            let state = app_state.read(cx);
            let Some(thread) = state.active_thread() else {
                return div().into_any_element();
            };
            let Some(message) = thread.messages.get(index) else {
                return div().into_any_element();
            };

            let bg = match message.role {
                Role::User => rgb(0x0e639c),
                Role::Agent => rgb(0x3c3c3c),
                Role::Thought => rgb(0x2d2d30),
                Role::System => rgb(0x6b2f2f),
            };
            let text_color = if message.role == Role::Thought {
                rgb(0xaaaaaa)
            } else {
                rgb(0xffffff)
            };

            let raw_content = message.content.clone();
            let copy_content = raw_content.to_string();

            // For now, we only collapse Text messages.
            let mut is_collapsible = false;
            let is_expanded = expanded_messages.contains(&message.id);
            let mut display_content = raw_content.clone();

            if let MessageContent::Text(text) = &raw_content {
                let line_count = text.lines().count();
                is_collapsible = line_count > COLLAPSE_LINE_LIMIT;
                if is_collapsible && !is_expanded {
                    display_content = MessageContent::Text(
                        text.lines()
                            .take(COLLAPSE_LINE_LIMIT)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }

            (
                bg,
                text_color,
                display_content,
                copy_content,
                message.thread_id,
                message.id,
                is_collapsible,
                is_expanded,
            )
        };

        let content_el = Self::render_message_content(
            app_state,
            thread_id,
            message_id,
            &message_content,
            window,
            cx,
        );

        let this_expand = this.clone();
        div()
            .id(("chat-message", index))
            .when_some(row_debug_selector(index), |this, selector| {
                this.debug_selector(move || selector.to_string())
            })
            .w_full()
            .min_w(px(0.0))
            .p_2()
            .cursor_text()
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_content.clone()));
            })
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .max_w_full()
                    .p_2()
                    .rounded_md()
                    .bg(bg)
                    .text_color(text_color)
                    .whitespace_normal()
                    .child(content_el)
                    .when(is_collapsible, |this| {
                        this.child(
                            div()
                                .id(("message-expand-toggle", index))
                                .mt_2()
                                .text_xs()
                                .text_color(rgb(0xbdbdbd))
                                .cursor_pointer()
                                .child(if is_expanded {
                                    "Show less"
                                } else {
                                    "Show more"
                                })
                                .on_click(move |_, _, cx| {
                                    this_expand.update(cx, |view, cx| {
                                        if view.expanded_messages.contains(&message_id) {
                                            view.expanded_messages.remove(&message_id);
                                        } else {
                                            view.expanded_messages.insert(message_id);
                                        }
                                        cx.notify();
                                    });
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_scroll_offset(&self) -> Point<Pixels> {
        // Negate Y to match the positive-increases-downward convention of ScrollHandle::offset().
        let p = self.list_state.scroll_px_offset_for_scrollbar();
        point(p.x, -p.y)
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_max_offset(&self) -> Size<Pixels> {
        self.list_state.max_offset_for_scrollbar()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_set_scroll_offset(
        &mut self,
        offset: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.list_state.set_offset_from_scrollbar(offset);
        self.update_scroll_lock(cx);
        cx.notify();
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_scroll_offset(&self, _message_id: Uuid) -> Option<Point<Pixels>> {
        None
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_max_offset(&self, _message_id: Uuid) -> Option<Size<Pixels>> {
        None
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_set_scroll_offset(&self, _message_id: Uuid, _offset: Point<Pixels>) -> bool {
        false
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_is_locked_to_bottom(&self) -> bool {
        self.locked_to_bottom
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_app_state(&self) -> Entity<AppState> {
        self.app_state.clone()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_input_value(&self, cx: &App) -> String {
        self.input_state.read(cx).value().to_string()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_focus_input(this: &Entity<ChatView>, window: &mut Window, cx: &mut App) {
        this.update(cx, |view, cx| {
            view.input_state.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        });
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_cursor(&self, cx: &App) -> usize {
        self.input_state.read(cx).cursor()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_set_cursor(
        this: &Entity<ChatView>,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        this.update(cx, |view, cx| {
            view.input_state.update(cx, |state, cx| {
                let position = state.text().offset_to_position(offset);
                state.set_cursor_position(position, window, cx);
            });
        });
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The scroll_handler callback fires while list_state is borrow_mut'd,
        // so it only sets user_scrolled=true. We call update_scroll_lock() here
        // where it is safe (no active borrow on list_state).
        if self.user_scrolled {
            self.user_scrolled = false;
            self.update_scroll_lock(cx);
        }
        // Maintain the bottom-lock invariant on every render.
        if self.locked_to_bottom {
            self.scroll_to_bottom();
        }

        let (
            active_thread_id,
            message_count,
            permission_options,
            config_options,
            modes,
            models,
            usage,
            is_working,
            configured_agents,
            selected_agent,
            is_agent_locked,
            locked_agent,
        ) = {
            let state = self.app_state.read(cx);
            (
                state.active_thread_id,
                state.active_thread_message_count(),
                state.active_thread_permission_options(),
                state.active_thread_config_options(),
                state.active_thread_modes(),
                state.active_thread_models(),
                state.active_thread_usage(),
                state.active_thread_is_working(),
                state.configured_agents().to_vec(),
                state.active_thread_selected_agent(),
                state.active_thread_is_agent_locked(),
                state.active_thread_locked_agent(),
            )
        };

        let total_count = message_count + is_working as usize;

        // Update the ListState only when the thread or item count changes so we
        // don't invalidate cached item heights unnecessarily on every render.
        match (
            active_thread_id != self.listed_thread_id,
            total_count != self.listed_count,
        ) {
            (true, _) => {
                let draft = active_thread_id
                    .and_then(|id| self.app_state.read(cx).thread_draft(id))
                    .unwrap_or_default();
                self.set_input_value(draft, _window, cx);

                let (scroll_offset, _saved_locked) = active_thread_id
                    .map(|id| self.app_state.read(cx).thread_scroll_state(id))
                    .unwrap_or((None, true));

                self.list_state.reset(total_count);

                // Re-engage scroll lock on every thread switch.
                self.locked_to_bottom = true;

                // Ensure we don't process a stale scroll event from the previous thread
                // that might overwrite our restored lock state.
                self.user_scrolled = false;

                if let Some(offset) = scroll_offset
                    && !_saved_locked
                {
                    self.list_state.set_offset_from_scrollbar(offset);
                    // The test 'thread_switch_re_engages_scroll_lock' expects that
                    // even if we had an offset, switching back re-locks it.
                    // If we want to restore "unlocked" state, we'd set self.locked_to_bottom = false here.
                    // But the test says: "switching back to thread A should re-engage scroll lock".
                    // So we stay locked.
                }

                self.listed_thread_id = active_thread_id;
            }
            (false, true) if total_count > self.listed_count => {
                // New items appended (new message or working sentinel added).
                self.list_state.splice(
                    self.listed_count..self.listed_count,
                    total_count - self.listed_count,
                );
            }
            (false, true) => {
                // Items removed (e.g. working sentinel cleared).
                self.list_state.reset(total_count);
            }
            _ => {
                // Same thread, same count: streaming chunk landed in the last
                // message. Invalidate just that item's cached height.
                if message_count > 0 {
                    self.list_state.splice(message_count - 1..message_count, 1);
                }
            }
        }
        self.listed_count = total_count;

        // Build list render closure — captures cheap handles, not the full messages.
        let app_state = self.app_state.clone();
        let this = cx.entity();
        let expanded_messages = self.expanded_messages.clone();

        let chat_content: AnyElement = if active_thread_id.is_some() {
            if total_count == 0 {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x888888))
                    .child("Send a message to start")
                    .into_any_element()
            } else {
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .debug_selector(|| "chat-message-list-container".to_string())
                    .child(
                        div()
                            .size_full()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .debug_selector(|| "chat-message-list-scrollable".to_string())
                            .child(
                                list(
                                    self.list_state.clone(),
                                    move |index, window, cx| -> AnyElement {
                                        if index >= message_count {
                                            // Working skeleton row.
                                            return div()
                                                .id(("chat-working", index))
                                                .debug_selector(|| "chat-working-row".to_string())
                                                .px_3()
                                                .py_2()
                                                .child(
                                                    div()
                                                        .p_3()
                                                        .rounded_md()
                                                        .bg(rgb(0x2d2d30))
                                                        .child(Skeleton::new()),
                                                )
                                                .into_any_element();
                                        }
                                        ChatView::render_message_row(
                                            index,
                                            &app_state,
                                            &this,
                                            &expanded_messages,
                                            window,
                                            cx,
                                        )
                                    },
                                )
                                .size_full()
                                .min_w(px(0.0))
                                .min_h(px(0.0)),
                            ),
                    )
                    .into_any_element()
            }
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0x888888))
                .child("Select or create a thread to begin")
                .into_any_element()
        };

        let usage_footer = usage.map(|usage| {
            let percent = usage_progress_percent(&usage);
            let progress_color = if percent >= 90.0 {
                rgb(0xd96b6b)
            } else if percent >= 75.0 {
                rgb(0xd9ad6b)
            } else {
                rgb(0x0e639c)
            };
            let percent_label = format!("{percent:.0}%");
            div()
                .id("chat-usage-indicator")
                .w_full()
                .flex()
                .justify_end()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f8f8f))
                        .child(format!("{} / {}", usage.used, usage.size)),
                )
                .when_some(format_cost_label(usage.cost.as_ref()), |this, label| {
                    this.child(
                        div()
                            .id("chat-usage-cost")
                            .text_xs()
                            .text_color(rgb(0xb8b8b8))
                            .child(label),
                    )
                })
                .child(
                    div()
                        .id("chat-usage-progress-circle")
                        .w(px(34.0))
                        .h(px(34.0))
                        .rounded_full()
                        .border_2()
                        .border_color(progress_color)
                        .bg(rgb(0x252526))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(white())
                        .child(percent_label),
                )
        });

        let input_box = div()
            .debug_selector(|| "chat-input-box".to_string())
            .w_full()
            .p_3()
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3c3c3c))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(36.0))
                            .max_h(px(250.0))
                            .child(Input::new(&self.input_state)),
                    )
                    .child(
                        div()
                            .id("send-button")
                            .debug_selector(|| "chat-send-button".to_string())
                            .bg(rgb(0x0e639c))
                            .text_color(white())
                            .rounded_md()
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .child("Send")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_input(window, cx);
                            })),
                    ),
            )
            .children(usage_footer);

        let suggestion_panel = if let Some(suggestions) = self.compute_suggestions(cx) {
            self.suggestion_scroll_handle
                .scroll_to_item(suggestions.selected);
            let rows: Vec<AnyElement> = suggestions
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    div()
                        .id(("suggestion-item", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(if index == suggestions.selected {
                            rgb(0x0e639c)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_color(white())
                        .child(item.display.clone())
                        .into_any_element()
                })
                .collect();
            div()
                .id("chat-suggestion-list")
                .w_full()
                .overflow_y_scroll()
                .track_scroll(&self.suggestion_scroll_handle)
                .max_h(px(180.0))
                .p_2()
                .bg(rgb(0x252526))
                .border_t_1()
                .border_color(rgb(0x3c3c3c))
                .children(rows)
        } else {
            div().id("chat-suggestion-empty")
        };

        let permission_panel = match (active_thread_id, permission_options) {
            (Some(thread_id), Some(options)) if !options.is_empty() => {
                let option_buttons = options.into_iter().enumerate().map(|(index, option)| {
                    let option_id = option.option_id.to_string();
                    div()
                        .id(("permission-option", index))
                        .bg(rgb(0x0e639c))
                        .text_color(white())
                        .rounded_md()
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .child(option.name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.app_state.update(cx, |state, cx| {
                                state.resolve_permission(cx, thread_id, Some(option_id.clone()));
                            });
                        }))
                });
                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(0x2d2d30))
                    .border_t_1()
                    .border_color(rgb(0x3c3c3c))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(rgb(0xdddddd)).child("Permission required"))
                    .children(option_buttons)
                    .child(
                        div()
                            .id("permission-cancel-button")
                            .bg(rgb(0x6b2f2f))
                            .text_color(white())
                            .rounded_md()
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    state.resolve_permission(cx, thread_id, None);
                                });
                            })),
                    )
            }
            _ => div(),
        };

        let config_panel = match (active_thread_id, config_options) {
            (Some(thread_id), Some(options)) if !options.is_empty() => {
                let option_rows = options
                    .into_iter()
                    .enumerate()
                    .map(|(option_index, option)| {
                        render_config_option_row(cx, thread_id, option_index, option)
                    });
                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(0x1f2933))
                    .border_t_1()
                    .border_color(rgb(0x3c3c3c))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(rgb(0xdddddd)).child("Session options"))
                    .children(option_rows)
            }
            _ => div(),
        };

        let model_panel = match (active_thread_id, models) {
            (
                Some(_thread_id),
                Some(SessionModelState {
                    current_model_id,
                    available_models,
                    ..
                }),
            ) if !available_models.is_empty() => {
                let options = available_models
                    .into_iter()
                    .map(|model| PickerOption::new(model.model_id.to_string(), model.name))
                    .collect::<Vec<_>>();
                let current_model_id = current_model_id.to_string();
                self.model_select_state.update(cx, |state, cx| {
                    state.set_items(options, _window, cx);
                    state.set_selected_value(&current_model_id, _window, cx);
                });
                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(0x1f2933))
                    .border_t_1()
                    .border_color(rgb(0x3c3c3c))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(rgb(0xdddddd)).child("Session model"))
                    .child(
                        div()
                            .id("chat-model-select")
                            .child(Select::new(&self.model_select_state).menu_width(px(260.0))),
                    )
            }
            _ => div(),
        };

        let mode_panel = match (active_thread_id, modes) {
            (
                Some(_thread_id),
                Some(SessionModeState {
                    current_mode_id,
                    available_modes,
                    ..
                }),
            ) if !available_modes.is_empty() => {
                let options = available_modes
                    .into_iter()
                    .map(|mode| PickerOption::new(mode.id.to_string(), mode.name))
                    .collect::<Vec<_>>();
                let current_mode_id = current_mode_id.to_string();
                self.mode_select_state.update(cx, |state, cx| {
                    state.set_items(options, _window, cx);
                    state.set_selected_value(&current_mode_id, _window, cx);
                });
                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(0x1f2933))
                    .border_t_1()
                    .border_color(rgb(0x3c3c3c))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(rgb(0xdddddd)).child("Session mode"))
                    .child(
                        div()
                            .id("chat-mode-select")
                            .child(Select::new(&self.mode_select_state).menu_width(px(220.0))),
                    )
            }
            _ => div(),
        };

        // Agent panel: shown below the input when there are configured agents.
        // - Thread unlocked: clickable selector buttons (one per configured agent).
        // - Thread locked: static label showing which agent is in use.
        let agent_panel = if !configured_agents.is_empty() {
            if let Some(thread_id) = active_thread_id {
                let panel_content: AnyElement = if is_agent_locked {
                    // Static label
                    let label = locked_agent
                        .as_deref()
                        .unwrap_or("unknown agent")
                        .to_string();
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_color(rgb(0x888888)).child("Agent"))
                        .child(div().text_color(rgb(0xdddddd)).child(label))
                        .into_any_element()
                } else {
                    // Clickable agent selector buttons
                    let buttons =
                        configured_agents
                            .into_iter()
                            .enumerate()
                            .map(|(index, agent)| {
                                let agent_name = agent.name.clone();
                                let is_selected = selected_agent.as_deref() == Some(&agent_name);
                                div()
                                    .id(("agent-selector", index))
                                    .bg(if is_selected {
                                        rgb(0x0e639c)
                                    } else {
                                        rgb(0x3c3c3c)
                                    })
                                    .text_color(white())
                                    .rounded_md()
                                    .px_2()
                                    .py_1()
                                    .cursor_pointer()
                                    .child(agent.name)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.app_state.update(cx, |state, cx| {
                                            state.select_agent_for_thread(
                                                cx,
                                                thread_id,
                                                agent_name.clone(),
                                            );
                                        });
                                    }))
                                    .into_any_element()
                            });
                    div()
                        .flex()
                        .gap_1()
                        .flex_wrap()
                        .children(buttons)
                        .into_any_element()
                };

                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(0x1f2933))
                    .border_t_1()
                    .border_color(rgb(0x3c3c3c))
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(rgb(0x888888)).child("Agent:"))
                    .child(panel_content)
            } else {
                div()
            }
        } else {
            div()
        };

        div()
            .debug_selector(|| "chat-root".to_string())
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .child(chat_content)
            .child(permission_panel)
            .child(suggestion_panel)
            .child(input_box)
            .child(agent_panel)
            .child(model_panel)
            .child(mode_panel)
            .child(config_panel)
    }
}

fn usage_progress_percent(usage: &UsageUpdate) -> f32 {
    if usage.size == 0 {
        return 0.0;
    }
    ((usage.used as f32 / usage.size as f32) * 100.0).clamp(0.0, 100.0)
}

fn format_cost_label(cost: Option<&Cost>) -> Option<String> {
    let cost = cost?;
    Some(format!("{:.2} {}", cost.amount, cost.currency))
}

fn slash_suggestion_items(commands: &[AvailableCommand], query: &str) -> Vec<SuggestionItem> {
    let query = query.to_lowercase();
    commands
        .iter()
        .filter(|command| {
            query.is_empty()
                || command.name.to_lowercase().contains(&query)
                || command.description.to_lowercase().contains(&query)
        })
        .map(|command| SuggestionItem {
            display: format!("/{} — {}", command.name, command.description),
            replacement: format!("/{} ", command.name),
        })
        .take(100)
        .collect()
}

fn file_suggestion_items(files: &[String], query: &str) -> Vec<SuggestionItem> {
    let query = query.to_lowercase();
    files
        .iter()
        .filter(|path| query.is_empty() || path.to_lowercase().contains(&query))
        .map(|path| SuggestionItem {
            display: path.clone(),
            replacement: format!("@{} ", path),
        })
        .take(100)
        .collect()
}

pub fn row_debug_selector(index: usize) -> Option<&'static str> {
    const SELECTORS: [&str; 64] = [
        "chat-row-0",
        "chat-row-1",
        "chat-row-2",
        "chat-row-3",
        "chat-row-4",
        "chat-row-5",
        "chat-row-6",
        "chat-row-7",
        "chat-row-8",
        "chat-row-9",
        "chat-row-10",
        "chat-row-11",
        "chat-row-12",
        "chat-row-13",
        "chat-row-14",
        "chat-row-15",
        "chat-row-16",
        "chat-row-17",
        "chat-row-18",
        "chat-row-19",
        "chat-row-20",
        "chat-row-21",
        "chat-row-22",
        "chat-row-23",
        "chat-row-24",
        "chat-row-25",
        "chat-row-26",
        "chat-row-27",
        "chat-row-28",
        "chat-row-29",
        "chat-row-30",
        "chat-row-31",
        "chat-row-32",
        "chat-row-33",
        "chat-row-34",
        "chat-row-35",
        "chat-row-36",
        "chat-row-37",
        "chat-row-38",
        "chat-row-39",
        "chat-row-40",
        "chat-row-41",
        "chat-row-42",
        "chat-row-43",
        "chat-row-44",
        "chat-row-45",
        "chat-row-46",
        "chat-row-47",
        "chat-row-48",
        "chat-row-49",
        "chat-row-50",
        "chat-row-51",
        "chat-row-52",
        "chat-row-53",
        "chat-row-54",
        "chat-row-55",
        "chat-row-56",
        "chat-row-57",
        "chat-row-58",
        "chat-row-59",
        "chat-row-60",
        "chat-row-61",
        "chat-row-62",
        "chat-row-63",
    ];
    SELECTORS.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn row_debug_selectors_cover_expected_range() {
        assert_eq!(row_debug_selector(0), Some("chat-row-0"));
        assert_eq!(row_debug_selector(63), Some("chat-row-63"));
        assert_eq!(row_debug_selector(64), None);
    }
}
