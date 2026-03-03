use crate::domain::{Message, Role};
use crate::state::AppState;
use agent_client_protocol::{AvailableCommand, SessionModeState};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::skeleton::Skeleton;
use gpui_component::text::TextView;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use std::collections::HashSet;
use std::rc::Rc;
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

struct SuggestionState {
    kind: SuggestionKind,
    start: usize,
    items: Vec<SuggestionItem>,
    selected: usize,
}

#[derive(Clone)]
enum ChatRow {
    Message(Message),
    Working,
}

pub struct ChatView {
    app_state: Entity<AppState>,
    scroll_handle: VirtualListScrollHandle,
    suggestion_scroll_handle: ScrollHandle,
    input_state: Entity<InputState>,
    locked_to_bottom: bool,
    suggestion_anchor: Option<(SuggestionKind, usize)>,
    suggestion_selected: usize,
    dismissed_suggestion: Option<(SuggestionKind, usize)>,
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    expanded_messages: HashSet<Uuid>,
    rows: Vec<ChatRow>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("Type and press Enter...")
        });

        cx.observe(&app_state, |this, _, cx| {
            if this.locked_to_bottom {
                this.scroll_handle.scroll_to_bottom();
            }
            cx.notify();
        })
        .detach();

        cx.subscribe(&input_state, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.history_cursor = None;
                this.reconcile_suggestion_visibility(cx);
                cx.notify();
            }
        })
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
            if key == "enter"
                && !event.keystroke.modifiers.shift
                && !event.keystroke.modifiers.secondary()
            {
                this.submit_input(window, cx);
            }
        })
        .detach();

        Self {
            app_state,
            scroll_handle: VirtualListScrollHandle::new(),
            suggestion_scroll_handle: ScrollHandle::new(),
            input_state,
            locked_to_bottom: true,
            suggestion_anchor: None,
            suggestion_selected: 0,
            dismissed_suggestion: None,
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            expanded_messages: HashSet::new(),
            rows: Vec::new(),
        }
    }

    fn input_value(&self, cx: &Context<Self>) -> String {
        self.input_state.read(cx).value().to_string()
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

    fn update_scroll_lock(&mut self) {
        let max_y = self.scroll_handle.max_offset().height;
        let y = self.scroll_handle.offset().y;
        self.locked_to_bottom = (max_y - y).abs() <= px(2.0);
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
                let Some(thread_id) = self.app_state.read(cx).active_thread_id else {
                    return None;
                };
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

    fn build_rows(&self, messages: Vec<Message>, is_working: bool) -> Vec<ChatRow> {
        let mut rows = messages
            .into_iter()
            .map(ChatRow::Message)
            .collect::<Vec<_>>();
        if is_working {
            rows.push(ChatRow::Working);
        }
        rows
    }

    fn estimate_row_size(row: &ChatRow) -> Size<Pixels> {
        let line_height = 20.0;
        let base = 24.0;
        let height = match row {
            ChatRow::Working => 56.0,
            ChatRow::Message(message) => {
                let lines = message.content.lines().count().max(1).min(14) as f32;
                base + lines * line_height
            }
        };
        size(px(1.0), px(height))
    }

    fn render_readonly_code(
        &self,
        message_id: Uuid,
        language: &str,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let language = language.to_string();
        let editor = window.use_keyed_state(
            SharedString::from(format!("chat-code-{}", message_id)),
            cx,
            |window, cx| {
                InputState::new(window, cx)
                    .code_editor(language.clone())
                    .line_number(true)
                    .rows(10)
            },
        );
        editor.update(cx, |state, cx| {
            state.set_value(content, window, cx);
        });
        div()
            .w_full()
            .h(px(220.0))
            .overflow_hidden()
            .child(Input::new(&editor).h_full().disabled(true))
            .into_any_element()
    }

    fn render_message_content(
        &self,
        message: &Message,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if content.contains("\n--- before\n+++ after") {
            return self.render_readonly_code(message.id, "diff", content, window, cx);
        }

        TextView::markdown(
            SharedString::from(format!("chat-md-{}", message.id)),
            SharedString::from(content),
            window,
            cx,
        )
        .selectable(true)
        .into_any_element()
    }

    fn render_row(
        &self,
        row_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(row_index).cloned() else {
            return div().into_any_element();
        };

        match row {
            ChatRow::Working => div()
                .id(("chat-working", row_index))
                .px_3()
                .py_2()
                .child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(0x2d2d30))
                        .child(Skeleton::new()),
                )
                .into_any_element(),
            ChatRow::Message(message) => {
                let bg = match message.role {
                    Role::User => rgb(0x0e639c),
                    Role::Agent => rgb(0x3c3c3c),
                    Role::System => rgb(0x6b2f2f),
                };
                let line_count = message.content.lines().count();
                let is_collapsible = line_count > 10;
                let is_expanded = self.expanded_messages.contains(&message.id);
                let display_content = if is_collapsible && !is_expanded {
                    message
                        .content
                        .lines()
                        .take(10)
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    message.content.clone()
                };
                let copy_content = message.content.clone();

                div()
                    .id(("chat-message", row_index))
                    .p_2()
                    .cursor_text()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_content.clone()));
                    }))
                    .child(
                        div()
                            .w_full()
                            .max_w_full()
                            .p_2()
                            .rounded_md()
                            .bg(bg)
                            .text_color(white())
                            .whitespace_normal()
                            .child(self.render_message_content(
                                &message,
                                display_content,
                                window,
                                cx,
                            ))
                            .when(is_collapsible, |this| {
                                let message_id = message.id;
                                this.child(
                                    div()
                                        .id(("message-expand-toggle", row_index))
                                        .mt_2()
                                        .text_xs()
                                        .text_color(rgb(0xbdbdbd))
                                        .cursor_pointer()
                                        .child(if is_expanded {
                                            "Show less"
                                        } else {
                                            "Show more"
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if this.expanded_messages.contains(&message_id) {
                                                this.expanded_messages.remove(&message_id);
                                            } else {
                                                this.expanded_messages.insert(message_id);
                                            }
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_scroll_lock();

        let (active_thread_id, messages, permission_options, config_options, modes, is_working) = {
            let state = self.app_state.read(cx);
            (
                state.active_thread_id,
                state.active_thread_messages(),
                state.active_thread_permission_options(),
                state.active_thread_config_options(),
                state.active_thread_modes(),
                state.active_thread_is_working(),
            )
        };

        self.rows = self.build_rows(messages, is_working);

        let chat_content: AnyElement = if active_thread_id.is_some() {
            if self.rows.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x888888))
                    .child("Send a message to start")
                    .into_any_element()
            } else {
                let item_sizes = Rc::new(
                    self.rows
                        .iter()
                        .map(Self::estimate_row_size)
                        .collect::<Vec<_>>(),
                );
                v_virtual_list(
                    cx.entity().clone(),
                    "chat-message-list",
                    item_sizes,
                    |this, range, window, cx| {
                        range
                            .map(|idx| this.render_row(idx, window, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll_handle)
                .flex_1()
                .w_full()
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

        let input_box = div()
            .w_full()
            .p_3()
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3c3c3c))
            .flex()
            .gap_2()
            .items_end()
            .child(
                div()
                    .flex_1()
                    .min_h(px(64.0))
                    .max_h(px(200.0))
                    .child(Input::new(&self.input_state).h_full()),
            )
            .child(
                div()
                    .id("send-button")
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
            );

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

        let mode_panel = match (active_thread_id, modes) {
            (
                Some(thread_id),
                Some(SessionModeState {
                    current_mode_id,
                    available_modes,
                    ..
                }),
            ) if !available_modes.is_empty() => {
                let buttons = available_modes
                    .into_iter()
                    .enumerate()
                    .map(|(index, mode)| {
                        let mode_id = mode.id.to_string();
                        let is_current = mode_id == current_mode_id.to_string();
                        div()
                            .id(("session-mode", index))
                            .bg(if is_current {
                                rgb(0x0e639c)
                            } else {
                                rgb(0x3c3c3c)
                            })
                            .text_color(white())
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .child(mode.name)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    state.set_session_mode(cx, thread_id, mode_id.clone());
                                });
                            }))
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
                    .child(div().flex().gap_1().flex_wrap().children(buttons))
            }
            _ => div(),
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(chat_content)
            .child(permission_panel)
            .child(suggestion_panel)
            .child(input_box)
            .child(mode_panel)
            .child(config_panel)
    }
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
