use crate::domain::{Message, Role};
use crate::state::AppState;
use agent_client_protocol::{AvailableCommand, SessionModeState};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::scroll::Scrollbar;
use gpui_component::skeleton::Skeleton;
use gpui_component::text::TextView;
use std::collections::{HashMap, HashSet};
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

const COLLAPSE_LINE_LIMIT: usize = 10;

pub struct ChatView {
    app_state: Entity<AppState>,
    scroll_handle: ScrollHandle,
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
    diff_scroll_handles: HashMap<Uuid, ScrollHandle>,
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
                this.scroll_to_bottom();
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
            scroll_handle: ScrollHandle::new(),
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
            diff_scroll_handles: HashMap::new(),
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

    fn scroll_to_bottom(&self) {
        let max = self.scroll_handle.max_offset();
        self.scroll_handle.set_offset(point(px(0.0), max.height));
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

    fn render_readonly_code(
        &mut self,
        message_id: Uuid,
        _language: &str,
        content: String,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let scroll_handle = window
            .use_keyed_state(
                SharedString::from(format!("chat-diff-scroll-{message_id}")),
                _cx,
                |_, _| ScrollHandle::new(),
            )
            .read(_cx)
            .clone();
        self.diff_scroll_handles
            .insert(message_id, scroll_handle.clone());
        let mut lines = Vec::new();
        let mut max_chars = 1usize;
        for (index, line) in content.lines().enumerate() {
            let text = if line.is_empty() { " " } else { line };
            max_chars = max_chars.max(text.chars().count());
            let row = div()
                .whitespace_nowrap()
                .child(text.to_owned())
                .when_some(diff_line_debug_selector(index), |this, selector| {
                    this.debug_selector(move || selector.to_string())
                });
            lines.push(row.into_any_element());
        }
        let content_width = px((max_chars as f32 * 8.0).max(480.0));
        div()
            .w_full()
            .h(px(220.0))
            .relative()
            .child(
                div()
                    .id(SharedString::from(format!("chat-diff-scroll-{message_id}")))
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .track_scroll(&scroll_handle)
                    .debug_selector(|| "chat-diff-scrollable".to_string())
                    .child(
                        div()
                            .w(content_width)
                            .min_w_full()
                            .flex()
                            .flex_col()
                            .p_2()
                            .font_family(".SystemUIFontMonospaced")
                            .text_sm()
                            .debug_selector(|| "chat-diff-content".to_string())
                            .children(lines),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .debug_selector(|| "chat-diff-scrollbar-v".to_string())
                    .child(Scrollbar::vertical(&scroll_handle)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .debug_selector(|| "chat-diff-scrollbar-h".to_string())
                    .child(Scrollbar::horizontal(&scroll_handle)),
            )
            .into_any_element()
    }

    fn render_message_content(
        &mut self,
        message: &Message,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if content.contains("--- before\n+++ after") {
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
        &mut self,
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
                .debug_selector(|| "chat-working-row".to_string())
                .when_some(row_debug_selector(row_index), |this, selector| {
                    this.debug_selector(move || selector.to_string())
                })
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
                let is_diff_message = message.content.contains("--- before\n+++ after");
                let is_collapsible = line_count > COLLAPSE_LINE_LIMIT && !is_diff_message;
                let is_expanded = self.expanded_messages.contains(&message.id);
                let display_content = if is_collapsible && !is_expanded {
                    message
                        .content
                        .lines()
                        .take(COLLAPSE_LINE_LIMIT)
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    message.content.clone()
                };
                let copy_content = message.content.clone();

                div()
                    .id(("chat-message", row_index))
                    .when_some(row_debug_selector(row_index), |this, selector| {
                        this.debug_selector(move || selector.to_string())
                    })
                    .w_full()
                    .min_w(px(0.0))
                    .p_2()
                    .cursor_text()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_content.clone()));
                    }))
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
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

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_scroll_offset(&self) -> Point<Pixels> {
        self.scroll_handle.offset()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_max_offset(&self) -> Size<Pixels> {
        self.scroll_handle.max_offset()
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_message_set_scroll_offset(&self, offset: Point<Pixels>) {
        self.scroll_handle.set_offset(offset);
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_scroll_offset(&self, message_id: Uuid) -> Option<Point<Pixels>> {
        self.diff_scroll_handles
            .get(&message_id)
            .map(ScrollHandle::offset)
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_max_offset(&self, message_id: Uuid) -> Option<Size<Pixels>> {
        self.diff_scroll_handles
            .get(&message_id)
            .map(ScrollHandle::max_offset)
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_set_scroll_offset(&self, message_id: Uuid, offset: Point<Pixels>) -> bool {
        if let Some(handle) = self.diff_scroll_handles.get(&message_id) {
            handle.set_offset(offset);
            true
        } else {
            false
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
                let rows = self.rows.len();
                let rows = (0..rows)
                    .map(|index| self.render_row(index, _window, cx))
                    .collect::<Vec<_>>();
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .debug_selector(|| "chat-message-list-container".to_string())
                    .child(
                        div()
                            .id("chat-message-list")
                            .size_full()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .overflow_x_hidden()
                            .track_scroll(&self.scroll_handle)
                            .vertical_scrollbar(&self.scroll_handle)
                            .debug_selector(|| "chat-message-list-scrollable".to_string())
                            .child(div().w_full().children(rows)),
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

        let input_box = div()
            .debug_selector(|| "chat-input-box".to_string())
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

fn row_debug_selector(index: usize) -> Option<&'static str> {
    const SELECTORS: [&str; 12] = [
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
    ];
    SELECTORS.get(index).copied()
}

fn diff_line_debug_selector(index: usize) -> Option<&'static str> {
    match index {
        0 => Some("chat-diff-line-0"),
        8 => Some("chat-diff-line-8"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn row_debug_selectors_cover_expected_range() {
        assert_eq!(row_debug_selector(0), Some("chat-row-0"));
        assert_eq!(row_debug_selector(11), Some("chat-row-11"));
        assert_eq!(row_debug_selector(12), None);
    }
}
