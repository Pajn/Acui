use crate::domain::Role;
use crate::state::AppState;
use agent_client_protocol::{AvailableCommand, SessionModeState};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState, RopeExt};
use gpui_component::scroll::Scrollbar;
use gpui_component::skeleton::Skeleton;
use gpui_component::text::TextView;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
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

struct SuggestionState {
    kind: SuggestionKind,
    start: usize,
    items: Vec<SuggestionItem>,
    selected: usize,
}

const COLLAPSE_LINE_LIMIT: usize = 10;

pub struct ChatView {
    app_state: Entity<AppState>,
    list_state: ListState,
    listed_thread_id: Option<Uuid>,
    listed_count: usize,
    suggestion_scroll_handle: ScrollHandle,
    input_state: Entity<InputState>,
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
    diff_scroll_handles: Rc<RefCell<std::collections::HashMap<Uuid, ScrollHandle>>>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("Type and press Enter to send, Shift+Enter for new line...")
        });

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
                // Set a flag; render() will call update_scroll_lock() safely.
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
            diff_scroll_handles: Rc::new(RefCell::new(std::collections::HashMap::new())),
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

    fn update_scroll_lock(&mut self) {
        // scroll_px_offset_for_scrollbar returns negative Y (content-moved-up convention),
        // so "at bottom" is when max_y + y ≈ 0.
        let max_y = self.list_state.max_offset_for_scrollbar().height;
        let y = self.list_state.scroll_px_offset_for_scrollbar().y;
        self.locked_to_bottom = (max_y + y).abs() <= px(2.0);
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

    fn render_readonly_code(
        message_id: Uuid,
        _language: &str,
        content: String,
        diff_scroll_handles: &Rc<RefCell<std::collections::HashMap<Uuid, ScrollHandle>>>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let scroll_handle = window
            .use_keyed_state(
                SharedString::from(format!("chat-diff-scroll-{message_id}")),
                cx,
                |_, _| ScrollHandle::new(),
            )
            .read(cx)
            .clone();
        diff_scroll_handles
            .borrow_mut()
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
        message_id: Uuid,
        content: String,
        diff_scroll_handles: &Rc<RefCell<std::collections::HashMap<Uuid, ScrollHandle>>>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        if content.contains("--- before\n+++ after") {
            return Self::render_readonly_code(
                message_id,
                "diff",
                content,
                diff_scroll_handles,
                window,
                cx,
            );
        }

        TextView::markdown(
            SharedString::from(format!("chat-md-{}", message_id)),
            SharedString::from(content),
            window,
            cx,
        )
        .selectable(true)
        .into_any_element()
    }

    /// Renders a single message row. Called from the list() render callback so it
    /// takes `Entity<AppState>` and `Entity<ChatView>` rather than `&mut self`.
    fn render_message_row(
        index: usize,
        app_state: &Entity<AppState>,
        this: &Entity<ChatView>,
        expanded_messages: &HashSet<Uuid>,
        diff_scroll_handles: &Rc<RefCell<std::collections::HashMap<Uuid, ScrollHandle>>>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let (
            bg,
            text_color,
            display_content,
            copy_content,
            message_id,
            is_collapsible,
            is_expanded,
        ): (Rgba, Rgba, String, String, Uuid, bool, bool) = {
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
            let line_count = message.content.lines().count();
            let is_diff_message = message.content.contains("--- before\n+++ after");
            let is_collapsible = line_count > COLLAPSE_LINE_LIMIT && !is_diff_message;
            let is_expanded = expanded_messages.contains(&message.id);
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
            (
                bg,
                text_color,
                display_content,
                message.content.clone(),
                message.id,
                is_collapsible,
                is_expanded,
            )
        };

        let content_el = Self::render_message_content(
            message_id,
            display_content,
            diff_scroll_handles,
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
        self.update_scroll_lock();
        cx.notify();
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_scroll_offset(&self, message_id: Uuid) -> Option<Point<Pixels>> {
        self.diff_scroll_handles
            .borrow()
            .get(&message_id)
            .map(ScrollHandle::offset)
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_max_offset(&self, message_id: Uuid) -> Option<Size<Pixels>> {
        self.diff_scroll_handles
            .borrow()
            .get(&message_id)
            .map(ScrollHandle::max_offset)
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_diff_set_scroll_offset(&self, message_id: Uuid, offset: Point<Pixels>) -> bool {
        if let Some(handle) = self.diff_scroll_handles.borrow().get(&message_id) {
            handle.set_offset(offset);
            true
        } else {
            false
        }
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
            self.update_scroll_lock();
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
                self.list_state.reset(total_count);
                // Always lock to bottom when switching threads so the new thread
                // starts scrolled to its most recent message.
                self.locked_to_bottom = true;
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
        self.listed_thread_id = active_thread_id;
        self.listed_count = total_count;

        // Build list render closure — captures cheap handles, not the full messages.
        let app_state = self.app_state.clone();
        let this = cx.entity();
        let expanded_messages = self.expanded_messages.clone();
        let diff_scroll_handles = self.diff_scroll_handles.clone();

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
                                            &diff_scroll_handles,
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

        // Agent panel: shown below the input when there are configured agents.
        // - Thread unlocked: clickable selector buttons (one per configured agent).
        // - Thread locked: static label showing which agent is in use.
        let agent_panel = if active_thread_id.is_some() && !configured_agents.is_empty() {
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
                let thread_id = active_thread_id.unwrap();
                let buttons = configured_agents
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
        assert_eq!(row_debug_selector(63), Some("chat-row-63"));
        assert_eq!(row_debug_selector(64), None);
    }
}
