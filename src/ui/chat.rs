use crate::domain::Role;
use crate::state::AppState;
use agent_client_protocol::{
    AvailableCommand, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use gpui::prelude::*;
use gpui::*;

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

pub struct ChatView {
    app_state: Entity<AppState>,
    scroll_handle: UniformListScrollHandle,
    input_buffer: String,
    locked_to_bottom: bool,
    suggestion_anchor: Option<(SuggestionKind, usize)>,
    suggestion_selected: usize,
    dismissed_suggestion: Option<(SuggestionKind, usize)>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |this, _, cx| {
            if this.locked_to_bottom {
                let count = this.app_state.read(cx).active_thread_message_count();
                if count > 0 {
                    this.scroll_handle
                        .scroll_to_item(count - 1, ScrollStrategy::Bottom);
                }
            }
            cx.notify();
        })
        .detach();

        cx.observe_keystrokes(|this, event, _window, cx| {
            if let Some(suggestions) = this.compute_suggestions(cx) {
                let key = event.keystroke.key.as_str();
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
                    this.apply_suggestion(&suggestions);
                    cx.notify();
                    return;
                }
            }

            if event.keystroke.key == "backspace" {
                this.input_buffer.pop();
                this.reconcile_suggestion_visibility();
                cx.notify();
                return;
            }

            if let Some(input) = event.keystroke.key_char.as_deref() {
                if input == "\n" {
                    this.submit_input(cx);
                    return;
                }

                if event.keystroke.modifiers.control {
                    return;
                }
                if event.keystroke.modifiers.alt || event.keystroke.modifiers.platform {
                    return;
                }
                if input.chars().any(|ch| !ch.is_control()) {
                    this.input_buffer.push_str(input);
                    this.reconcile_suggestion_visibility();
                    cx.notify();
                }
            }
        })
        .detach();

        Self {
            app_state,
            scroll_handle: UniformListScrollHandle::new(),
            input_buffer: String::new(),
            locked_to_bottom: true,
            suggestion_anchor: None,
            suggestion_selected: 0,
            dismissed_suggestion: None,
        }
    }

    fn submit_input(&mut self, cx: &mut Context<Self>) {
        let content = self.input_buffer.trim().to_owned();
        if content.is_empty() {
            return;
        }

        let thread_id = self.app_state.read(cx).active_thread_id;
        if let Some(thread_id) = thread_id {
            self.app_state.update(cx, |state, cx| {
                state.send_user_message(cx, thread_id, &content);
            });
            self.input_buffer.clear();
            self.locked_to_bottom = true;
            cx.notify();
        }
    }

    fn update_scroll_lock(&mut self) {
        let base = self.scroll_handle.0.borrow().base_handle.clone();
        let max_y = base.max_offset().height;
        let y = base.offset().y;
        self.locked_to_bottom = (max_y - y).abs() <= px(2.0);
    }

    fn suggestion_anchor(&self) -> Option<(SuggestionKind, usize, String)> {
        if self.input_buffer.starts_with('/') && !self.input_buffer.contains(char::is_whitespace) {
            return Some((SuggestionKind::Slash, 0, self.input_buffer[1..].to_string()));
        }

        let token_start = self
            .input_buffer
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        let token = &self.input_buffer[token_start..];
        token
            .strip_prefix('@')
            .map(|query| (SuggestionKind::File, token_start, query.to_string()))
    }

    fn reconcile_suggestion_visibility(&mut self) {
        if self.dismissed_suggestion.is_some()
            && self
                .suggestion_anchor()
                .map(|(kind, start, _)| (kind, start))
                != self.dismissed_suggestion
        {
            self.dismissed_suggestion = None;
        }
    }

    fn compute_suggestions(&mut self, cx: &Context<Self>) -> Option<SuggestionState> {
        self.reconcile_suggestion_visibility();
        let (kind, start, query) = self.suggestion_anchor()?;
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

    fn apply_suggestion(&mut self, state: &SuggestionState) {
        let Some(item) = state.items.get(state.selected) else {
            return;
        };
        self.input_buffer
            .replace_range(state.start..self.input_buffer.len(), &item.replacement);
        self.suggestion_anchor = None;
        self.dismissed_suggestion = None;
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_scroll_lock();

        let (active_thread_id, messages, permission_options, config_options) = {
            let state = self.app_state.read(cx);
            (
                state.active_thread_id,
                state.active_thread_messages(),
                state.active_thread_permission_options(),
                state.active_thread_config_options(),
            )
        };

        let chat_content = if active_thread_id.is_some() {
            if messages.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x888888))
                    .child("Send a message to start")
            } else {
                let message_count = messages.len();
                let items = messages;

                div().flex_1().w_full().child(
                    uniform_list("chat-message-list", message_count, move |range, _, _| {
                        range
                            .map(|i| {
                                let msg = &items[i];
                                let bg = match msg.role {
                                    Role::User => rgb(0x0e639c),
                                    Role::Agent => rgb(0x3c3c3c),
                                    Role::System => rgb(0x6b2f2f),
                                };

                                div().p_2().child(
                                    div()
                                        .p_2()
                                        .rounded_md()
                                        .bg(bg)
                                        .text_color(white())
                                        .child(msg.content.clone()),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(self.scroll_handle.clone())
                    .h_full(),
                )
            }
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0x888888))
                .child("Select or create a thread to begin")
        };

        let input_hint = if self.input_buffer.is_empty() {
            "Type and press Enter...".to_string()
        } else {
            self.input_buffer.clone()
        };

        let input_box = div()
            .h(px(64.0))
            .w_full()
            .p_3()
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3c3c3c))
            .flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .bg(rgb(0x3c3c3c))
                    .rounded_md()
                    .px_3()
                    .py_2()
                    .text_color(if self.input_buffer.is_empty() {
                        rgb(0x9a9a9a)
                    } else {
                        rgb(0xffffff)
                    })
                    .child(input_hint),
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
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.submit_input(cx);
                    })),
            );

        let suggestion_panel = if let Some(suggestions) = self.compute_suggestions(cx) {
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

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(chat_content)
            .child(permission_panel)
            .child(suggestion_panel)
            .child(input_box)
            .child(config_panel)
    }
}

fn render_config_option_row(
    cx: &Context<ChatView>,
    thread_id: uuid::Uuid,
    option_index: usize,
    option: SessionConfigOption,
) -> impl IntoElement {
    let option_id = option.id.to_string();
    let title = div().text_color(rgb(0xdddddd)).child(option.name);
    match option.kind {
        SessionConfigKind::Select(select) => {
            let entries = match select.options {
                SessionConfigSelectOptions::Ungrouped(values) => values
                    .into_iter()
                    .map(|entry| (entry.value.to_string(), entry.name))
                    .collect::<Vec<_>>(),
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .into_iter()
                    .flat_map(|group| {
                        group.options.into_iter().map(move |entry| {
                            (
                                entry.value.to_string(),
                                format!("{} / {}", group.group, entry.name),
                            )
                        })
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let current_value = select.current_value.to_string();
            let value_buttons =
                entries
                    .into_iter()
                    .enumerate()
                    .map(|(value_index, (value_id, name))| {
                        let is_active = value_id == current_value;
                        div()
                            .id(("session-config-value", option_index * 100 + value_index))
                            .bg(if is_active {
                                rgb(0x0e639c)
                            } else {
                                rgb(0x3c3c3c)
                            })
                            .text_color(white())
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .child(name)
                            .on_click(cx.listener({
                                let option_id = option_id.clone();
                                move |this, _, _, cx| {
                                    this.app_state.update(cx, |state, cx| {
                                        state.set_session_config_option(
                                            cx,
                                            thread_id,
                                            option_id.clone(),
                                            value_id.clone(),
                                        );
                                    });
                                }
                            }))
                    });
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(title)
                .child(div().flex().gap_1().flex_wrap().children(value_buttons))
        }
        _ => div().w_full().child(title),
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
        .collect()
}
