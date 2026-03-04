use crate::domain::{MessageContent, Role};
use crate::state::AppState;
use agent_client_protocol::{
    AvailableCommand, Cost, SessionModeState, SessionModelState, UsageUpdate,
};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState, RopeExt};
use gpui_component::scroll::{Scrollbar, ScrollbarShow};
use gpui_component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_component::skeleton::Skeleton;
use gpui_component::text::TextView;
use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use uuid::Uuid;

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
const COLLAPSE_CHAR_LIMIT: usize = 2_000;
const MARKDOWN_FASTPATH_LENGTH: usize = 2_000;
const VIRTUALIZE_LINE_THRESHOLD: usize = 256;
const VIRTUALIZE_CHAR_THRESHOLD: usize = 16_384;
const VIRTUALIZED_CHUNK_LINES: usize = 12;
const VIRTUALIZED_BLOCK_HEIGHT: f32 = 200.0;
const VIRTUALIZED_CODE_LINE_HEIGHT: f32 = 16.0;
const NON_SCROLL_RENDER_CHUNK_LINES: usize = 200;
const EDITOR_HIGHLIGHT_MAX_LINES: usize = 1_200;
const EDITOR_HIGHLIGHT_MAX_CHARS: usize = 256_000;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct TextFingerprint {
    len: usize,
    head: u64,
    mid: u64,
    tail: u64,
}

#[derive(Clone)]
struct VirtualizedCodeChunk {
    code_text: String,
    line_numbers: String,
    signs: String,
    line_count: usize,
    has_added: bool,
    has_removed: bool,
}

struct VirtualizedCodeCache {
    fingerprint: TextFingerprint,
    chunks: Arc<[VirtualizedCodeChunk]>,
}

impl Default for VirtualizedCodeCache {
    fn default() -> Self {
        Self {
            fingerprint: TextFingerprint::default(),
            chunks: Arc::from(Vec::<VirtualizedCodeChunk>::new()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffRowKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone)]
struct VirtualizedDiffRow {
    kind: DiffRowKind,
    sign: char,
    line_number_label: SharedString,
    text: SharedString,
}

#[derive(Clone)]
struct VirtualizedDiffBlock {
    kind: DiffRowKind,
    signs: SharedString,
    line_numbers: SharedString,
    text: SharedString,
}

#[derive(Clone)]
struct VirtualizedDiffChunk {
    blocks: Arc<[VirtualizedDiffBlock]>,
}

struct VirtualizedDiffCache {
    fingerprint: TextFingerprint,
    diff_text: String,
    chunks: Arc<[VirtualizedDiffChunk]>,
}

impl Default for VirtualizedDiffCache {
    fn default() -> Self {
        Self {
            fingerprint: TextFingerprint::default(),
            diff_text: String::new(),
            chunks: Arc::from(Vec::<VirtualizedDiffChunk>::new()),
        }
    }
}

struct VirtualizedMarkdownCache {
    fingerprint: TextFingerprint,
    chunks: Arc<[SharedString]>,
}

impl Default for VirtualizedMarkdownCache {
    fn default() -> Self {
        Self {
            fingerprint: TextFingerprint::default(),
            chunks: Arc::from(Vec::<SharedString>::new()),
        }
    }
}

struct VirtualizedListState {
    list_state: ListState,
    item_count: usize,
}

impl VirtualizedListState {
    fn new() -> Self {
        Self {
            list_state: ListState::new(0, ListAlignment::Top, px(VIRTUALIZED_BLOCK_HEIGHT)),
            item_count: 0,
        }
    }
}

#[derive(Default)]
struct MessageContentCache {
    signature: Option<MessageTailSignature>,
    content: Option<Arc<MessageContent>>,
}

#[derive(Clone, Copy)]
struct VirtualizedCodeRenderOptions {
    parse_numbered_lines: bool,
    prefer_editor: bool,
    inner_scroll: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MessageTailSignature {
    message_id: Uuid,
    content_size: usize,
    is_streaming: bool,
}

pub struct ChatView {
    app_state: Entity<AppState>,
    list_state: ListState,
    listed_thread_id: Option<Uuid>,
    listed_count: usize,
    last_tail_signature: Option<MessageTailSignature>,
    suggestion_scroll_handle: ScrollHandle,
    input_state: Entity<InputState>,
    mode_select_state: Entity<SelectState<Vec<PickerOption>>>,
    model_select_state: Entity<SelectState<Vec<PickerOption>>>,
    config_select_states: Vec<Entity<SelectState<Vec<PickerOption>>>>,
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
        let config_select_states: Vec<_> = (0..4)
            .map(|_| cx.new(|cx| SelectState::new(Vec::<PickerOption>::new(), None, window, cx)))
            .collect();

        for (idx, select_state) in config_select_states.iter().enumerate() {
            let idx_clone = idx;
            cx.subscribe(
                select_state,
                move |this, _state, event: &SelectEvent<Vec<PickerOption>>, cx| {
                    let SelectEvent::Confirm(Some(value)) = event else {
                        return;
                    };
                    if let Some(thread_id) = this.app_state.read(cx).active_thread_id
                        && let Some(option_id) = this
                            .app_state
                            .read(cx)
                            .active_thread_config_options()
                            .and_then(|opts| opts.get(idx_clone).map(|o| o.id.to_string()))
                    {
                        this.app_state.update(cx, |state, cx| {
                            state.set_session_config_option(
                                cx,
                                thread_id,
                                option_id,
                                value.clone(),
                            );
                        });
                    }
                },
            )
            .detach();
        }

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
            let state = ListState::new(0, ListAlignment::Bottom, px(512.0));
            let weak = this.downgrade();
            state.set_scroll_handler(move |event, _window, app| {
                // list_state.0 is borrow_mut'd when this handler fires, so we
                // must NOT call borrow() on it (e.g. via max_offset_for_scrollbar).
                // Set a flag; render() will process it safely.
                let _ = weak.update(app, |this, _cx| {
                    this.user_scrolled = true;
                    this.locked_to_bottom = !event.is_scrolled;
                });
            });
            state
        };

        Self {
            app_state,
            list_state,
            listed_thread_id: None,
            listed_count: 0,
            last_tail_signature: None,
            suggestion_scroll_handle: ScrollHandle::new(),
            input_state,
            mode_select_state,
            model_select_state,
            config_select_states,
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
        let offset = self.normalized_scroll_offset();
        if let Some(thread_id) = self.app_state.read(cx).active_thread_id {
            let locked = self.locked_to_bottom;
            self.app_state.update(cx, |state, _| {
                state.update_thread_scroll_state(thread_id, offset, locked);
            });
        }
    }

    fn normalized_scroll_offset(&self) -> Point<Pixels> {
        let raw_offset = self.list_state.scroll_px_offset_for_scrollbar();
        let max_y = self.list_state.max_offset_for_scrollbar().height;
        let y = if self.locked_to_bottom {
            max_y
        } else {
            raw_offset.y.abs().min(max_y)
        };
        point(px(0.0), y)
    }

    fn scroll_to_bottom(&self) {
        let max = self.list_state.max_offset_for_scrollbar();
        if max.height > px(0.0) {
            self.list_state
                .set_offset_from_scrollbar(point(px(0.0), max.height));
        }
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

    fn virtualized_list_state(
        key: SharedString,
        item_count: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> ListState {
        let list_state =
            window.use_keyed_state(key, cx, |_window, _cx| VirtualizedListState::new());
        list_state.update(cx, |state, _| {
            if state.item_count != item_count {
                state.list_state.reset(item_count);
                state.item_count = item_count;
            }
        });
        list_state.read(cx).list_state.clone()
    }

    fn render_readonly_code(
        message_id: Uuid,
        key_suffix: &str,
        language: &str,
        content: String,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let language = SharedString::from(language.to_string());
        let content_fingerprint = text_fingerprint(&content);
        let input_state = window
            .use_keyed_state(
                SharedString::from(format!(
                    "chat-code-input-{}-{key_suffix}-{}-{}-{}",
                    message_id,
                    content_fingerprint.len,
                    content_fingerprint.head,
                    content_fingerprint.tail
                )),
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .multi_line(true)
                            .code_editor(language)
                            .line_number(true)
                            .searchable(false)
                            .soft_wrap(false)
                            .default_value(content.clone())
                    })
                },
            )
            .read(cx)
            .clone();

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

    fn render_virtualized_code_chunk_editor(
        message_id: Uuid,
        block_key: &str,
        language: &str,
        chunk_index: usize,
        chunk: &VirtualizedCodeChunk,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let language = SharedString::from(language.to_string());
        let chunk_rows = chunk.line_count.max(1);
        let initial_value = chunk.code_text.clone();
        let chunk_fingerprint = text_fingerprint(&chunk.code_text);
        let input_state = window
            .use_keyed_state(
                SharedString::from(format!(
                    "chat-code-chunk-input-{message_id}-{block_key}-{chunk_index}-{}-{}-{}",
                    chunk_fingerprint.len, chunk_fingerprint.head, chunk_fingerprint.tail
                )),
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .multi_line(true)
                            .code_editor(language)
                            .line_number(false)
                            .rows(chunk_rows)
                            .searchable(false)
                            .soft_wrap(false)
                            .default_value(initial_value.clone())
                    })
                },
            )
            .read(cx)
            .clone();
        let content_height = px((chunk_rows as f32 * VIRTUALIZED_CODE_LINE_HEIGHT).max(16.0));

        let row_bg = if chunk.has_added && !chunk.has_removed {
            rgb(0x173221)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0x3a1f24)
        } else {
            rgb(0x1e1e1e)
        };
        let has_signs = chunk.has_added || chunk.has_removed;
        let sign_color = if chunk.has_added && !chunk.has_removed {
            rgb(0x82dca7)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0xf2a2a2)
        } else {
            rgb(0x909090)
        };

        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .items_start()
            .border_b_1()
            .border_color(rgb(0x2d2d30))
            .bg(row_bg)
            .when(has_signs, |this| {
                this.child(
                    div()
                        .w(px(18.0))
                        .px_1()
                        .py_1()
                        .text_xs()
                        .text_color(sign_color)
                        .whitespace_nowrap()
                        .child(chunk.signs.clone()),
                )
            })
            .child(
                div()
                    .w(px(64.0))
                    .px_1()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .whitespace_nowrap()
                    .child(chunk.line_numbers.clone()),
            )
            .child(
                div().flex_1().min_w(px(0.0)).h(content_height).child(
                    Input::new(&input_state)
                        .disabled(true)
                        .appearance(false)
                        .h_full(),
                ),
            )
            .into_any_element()
    }

    fn render_virtualized_code_chunk_plain(chunk: &VirtualizedCodeChunk) -> AnyElement {
        let row_bg = if chunk.has_added && !chunk.has_removed {
            rgb(0x173221)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0x3a1f24)
        } else {
            rgb(0x1e1e1e)
        };
        let has_signs = chunk.has_added || chunk.has_removed;
        let sign_color = if chunk.has_added && !chunk.has_removed {
            rgb(0x82dca7)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0xf2a2a2)
        } else {
            rgb(0x909090)
        };

        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .items_start()
            .border_b_1()
            .border_color(rgb(0x2d2d30))
            .bg(row_bg)
            .when(has_signs, |this| {
                this.child(
                    div()
                        .w(px(18.0))
                        .px_1()
                        .py_1()
                        .text_xs()
                        .text_color(sign_color)
                        .whitespace_nowrap()
                        .child(chunk.signs.clone()),
                )
            })
            .child(
                div()
                    .w(px(64.0))
                    .px_1()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .whitespace_nowrap()
                    .child(chunk.line_numbers.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .px_1()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0xd6d6d6))
                    .whitespace_nowrap()
                    .child(chunk.code_text.clone()),
            )
            .into_any_element()
    }

    fn render_virtualized_code_chunk_highlighted(
        message_id: Uuid,
        block_key: &str,
        language: &str,
        chunk_index: usize,
        chunk: &VirtualizedCodeChunk,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let row_bg = if chunk.has_added && !chunk.has_removed {
            rgb(0x173221)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0x3a1f24)
        } else {
            rgb(0x1e1e1e)
        };
        let has_signs = chunk.has_added || chunk.has_removed;
        let sign_color = if chunk.has_added && !chunk.has_removed {
            rgb(0x82dca7)
        } else if chunk.has_removed && !chunk.has_added {
            rgb(0xf2a2a2)
        } else {
            rgb(0x909090)
        };
        let chunk_fingerprint = text_fingerprint(&chunk.code_text);
        let markdown = format!("```{language}\n{}\n```", chunk.code_text);
        let markdown_key = SharedString::from(format!(
            "chat-code-highlighted-{message_id}-{block_key}-{chunk_index}-{}-{}-{}",
            chunk_fingerprint.len, chunk_fingerprint.head, chunk_fingerprint.tail
        ));

        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .items_start()
            .border_b_1()
            .border_color(rgb(0x2d2d30))
            .bg(row_bg)
            .when(has_signs, |this| {
                this.child(
                    div()
                        .w(px(18.0))
                        .px_1()
                        .py_1()
                        .text_xs()
                        .text_color(sign_color)
                        .whitespace_nowrap()
                        .child(chunk.signs.clone()),
                )
            })
            .child(
                div()
                    .w(px(64.0))
                    .px_1()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .whitespace_nowrap()
                    .child(chunk.line_numbers.clone()),
            )
            .child(
                div().flex_1().min_w(px(0.0)).px_1().py_1().child(
                    TextView::markdown(markdown_key, SharedString::from(markdown), window, cx)
                        .selectable(true),
                ),
            )
            .into_any_element()
    }

    fn render_virtualized_code_block(
        message_id: Uuid,
        block_key: &str,
        language: &str,
        text: &str,
        options: VirtualizedCodeRenderOptions,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let fingerprint = text_fingerprint(text);
        let cache = window.use_keyed_state(
            SharedString::from(format!("chat-code-cache-{message_id}-{block_key}")),
            cx,
            |_window, _cx| VirtualizedCodeCache::default(),
        );
        cache.update(cx, |state, _| {
            if state.fingerprint != fingerprint {
                state.fingerprint = fingerprint;
                state.chunks =
                    build_virtualized_code_chunks(text, options.parse_numbered_lines).into();
            }
        });
        let chunks = cache.read(cx).chunks.clone();
        if chunks.is_empty() {
            return div().into_any_element();
        }
        let total_lines = chunks.iter().map(|chunk| chunk.line_count).sum::<usize>();
        let use_editor = options.prefer_editor
            && total_lines <= EDITOR_HIGHLIGHT_MAX_LINES
            && text.len() <= EDITOR_HIGHLIGHT_MAX_CHARS;

        if use_editor
            && chunks.len() == 1
            && text.len() <= VIRTUALIZE_CHAR_THRESHOLD
            && options.inner_scroll
        {
            return Self::render_readonly_code(
                message_id,
                block_key,
                language,
                chunks[0].code_text.clone(),
                window,
                cx,
            );
        }

        let language = language.to_string();
        let block_key = block_key.to_string();
        let chunks_for_render = chunks.clone();

        if !options.inner_scroll {
            let render_chunks = if use_editor {
                build_non_scrolling_render_chunks(chunks_for_render.as_ref())
            } else {
                chunks_for_render.as_ref().to_vec()
            };
            return div()
                .w_full()
                .rounded_md()
                .border_1()
                .border_color(rgb(0x3c3c3c))
                .bg(rgb(0x1e1e1e))
                .overflow_hidden()
                .children(
                    render_chunks
                        .iter()
                        .enumerate()
                        .map(|(chunk_index, chunk)| {
                            if use_editor {
                                ChatView::render_virtualized_code_chunk_highlighted(
                                    message_id,
                                    &block_key,
                                    &language,
                                    chunk_index,
                                    chunk,
                                    window,
                                    cx,
                                )
                            } else {
                                ChatView::render_virtualized_code_chunk_plain(chunk)
                            }
                        }),
                )
                .into_any_element();
        }

        let list_state = Self::virtualized_list_state(
            SharedString::from(format!("chat-code-list-{message_id}-{block_key}")),
            chunks.len(),
            window,
            cx,
        );

        div()
            .w_full()
            .h(px(VIRTUALIZED_BLOCK_HEIGHT))
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3c3c3c))
            .bg(rgb(0x1e1e1e))
            .overflow_hidden()
            .child(
                list(list_state, move |chunk_index, window, cx| -> AnyElement {
                    let Some(chunk) = chunks_for_render.get(chunk_index) else {
                        return div().into_any_element();
                    };
                    if use_editor {
                        ChatView::render_virtualized_code_chunk_editor(
                            message_id,
                            &block_key,
                            &language,
                            chunk_index,
                            chunk,
                            window,
                            cx,
                        )
                    } else {
                        let _ = window;
                        let _ = cx;
                        ChatView::render_virtualized_code_chunk_plain(chunk)
                    }
                })
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0)),
            )
            .into_any_element()
    }

    fn render_virtualized_diff_chunk(blocks: &[VirtualizedDiffBlock]) -> AnyElement {
        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .children(blocks.iter().map(move |block| {
                let (bg, sign_color, text_color) = match block.kind {
                    DiffRowKind::Context => (rgb(0x1e1e1e), rgb(0x8f8f8f), rgb(0xd6d6d6)),
                    DiffRowKind::Added => (rgb(0x173221), rgb(0x82dca7), rgb(0xdaf4e3)),
                    DiffRowKind::Removed => (rgb(0x3a1f24), rgb(0xf2a2a2), rgb(0xf7dddd)),
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .items_start()
                    .px_1()
                    .bg(bg)
                    .child(
                        div()
                            .w(px(18.0))
                            .text_xs()
                            .text_color(sign_color)
                            .whitespace_nowrap()
                            .child(block.signs.clone()),
                    )
                    .child(
                        div()
                            .w(px(64.0))
                            .text_xs()
                            .text_color(rgb(0x8a8a8a))
                            .whitespace_nowrap()
                            .child(block.line_numbers.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(text_color)
                            .whitespace_nowrap()
                            .child(block.text.clone()),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_virtualized_diff(
        message_id: Uuid,
        block_key: &str,
        old_text: Option<&str>,
        new_text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let old_fingerprint = text_fingerprint(old_text.unwrap_or_default());
        let new_fingerprint = text_fingerprint(new_text);
        let fingerprint = TextFingerprint {
            len: old_fingerprint.len + new_fingerprint.len + 1,
            head: old_fingerprint.head ^ new_fingerprint.head.rotate_left(1),
            mid: old_fingerprint.mid ^ new_fingerprint.mid.rotate_left(3),
            tail: old_fingerprint.tail ^ new_fingerprint.tail.rotate_left(7),
        };
        let cache = window.use_keyed_state(
            SharedString::from(format!("chat-diff-cache-{message_id}-{block_key}")),
            cx,
            |_window, _cx| VirtualizedDiffCache::default(),
        );
        cache.update(cx, |state, _| {
            if state.fingerprint != fingerprint {
                state.fingerprint = fingerprint;
                state.diff_text = crate::state::render_diff_text(old_text, new_text);
                state.chunks = build_virtualized_diff_chunks(&state.diff_text).into();
            }
        });
        let chunks = cache.read(cx).chunks.clone();
        if chunks.is_empty() {
            return div().into_any_element();
        }

        let list_state = Self::virtualized_list_state(
            SharedString::from(format!("chat-diff-list-{message_id}-{block_key}")),
            chunks.len(),
            window,
            cx,
        );
        let chunks_for_render = chunks.clone();

        div()
            .w_full()
            .h(px(VIRTUALIZED_BLOCK_HEIGHT))
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3c3c3c))
            .bg(rgb(0x1e1e1e))
            .overflow_hidden()
            .child(
                list(list_state, move |chunk_index, _window, _cx| -> AnyElement {
                    let Some(chunk) = chunks_for_render.get(chunk_index) else {
                        return div().into_any_element();
                    };
                    ChatView::render_virtualized_diff_chunk(chunk.blocks.as_ref())
                })
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0)),
            )
            .into_any_element()
    }

    fn render_virtualized_markdown(
        message_id: Uuid,
        block_key: &str,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let fingerprint = text_fingerprint(text);
        let cache = window.use_keyed_state(
            SharedString::from(format!("chat-md-cache-{message_id}-{block_key}")),
            cx,
            |_window, _cx| VirtualizedMarkdownCache::default(),
        );
        cache.update(cx, |state, _| {
            if state.fingerprint != fingerprint {
                state.fingerprint = fingerprint;
                state.chunks = build_text_chunks(text).into();
            }
        });
        let chunks = cache.read(cx).chunks.clone();
        if chunks.is_empty() {
            return div().into_any_element();
        }
        let list_state = Self::virtualized_list_state(
            SharedString::from(format!("chat-md-list-{message_id}-{block_key}")),
            chunks.len(),
            window,
            cx,
        );
        let chunks_for_render = chunks.clone();
        let block_key = block_key.to_string();

        div()
            .w_full()
            .h(px(VIRTUALIZED_BLOCK_HEIGHT))
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3c3c3c))
            .bg(rgb(0x1e1e1e))
            .overflow_hidden()
            .child(
                list(list_state, move |chunk_index, window, cx| -> AnyElement {
                    let Some(chunk) = chunks_for_render.get(chunk_index) else {
                        return div().into_any_element();
                    };
                    div()
                        .px_2()
                        .py_1()
                        .child(
                            TextView::markdown(
                                SharedString::from(format!(
                                    "chat-virtual-md-{message_id}-{block_key}-{chunk_index}"
                                )),
                                chunk.clone(),
                                window,
                                cx,
                            )
                            .selectable(true),
                        )
                        .into_any_element()
                })
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0)),
            )
            .into_any_element()
    }

    fn render_virtualized_plain_text(
        message_id: Uuid,
        block_key: &str,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let fingerprint = text_fingerprint(text);
        let cache = window.use_keyed_state(
            SharedString::from(format!("chat-plain-cache-{message_id}-{block_key}")),
            cx,
            |_window, _cx| VirtualizedMarkdownCache::default(),
        );
        cache.update(cx, |state, _| {
            if state.fingerprint != fingerprint {
                state.fingerprint = fingerprint;
                state.chunks = build_text_chunks(text).into();
            }
        });
        let chunks = cache.read(cx).chunks.clone();
        if chunks.is_empty() {
            return div().into_any_element();
        }
        let list_state = Self::virtualized_list_state(
            SharedString::from(format!("chat-plain-list-{message_id}-{block_key}")),
            chunks.len(),
            window,
            cx,
        );
        let chunks_for_render = chunks.clone();

        div()
            .w_full()
            .h(px(VIRTUALIZED_BLOCK_HEIGHT))
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3c3c3c))
            .bg(rgb(0x1e1e1e))
            .overflow_hidden()
            .child(
                list(list_state, move |chunk_index, _window, _cx| -> AnyElement {
                    let Some(chunk) = chunks_for_render.get(chunk_index) else {
                        return div().into_any_element();
                    };
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        .px_2()
                        .py_1()
                        .whitespace_nowrap()
                        .child(chunk.clone())
                        .into_any_element()
                })
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0)),
            )
            .into_any_element()
    }

    fn render_markdown_or_plain_text(
        key: SharedString,
        text: String,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        if should_render_markdown(&text) {
            TextView::markdown(key, SharedString::from(text), window, cx)
                .selectable(true)
                .into_any_element()
        } else {
            div().w_full().min_w(px(0.0)).child(text).into_any_element()
        }
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
                        "message-diff",
                        "diff",
                        text.clone(),
                        window,
                        cx,
                    );
                }

                if should_virtualize_text(text) {
                    if should_render_markdown(text) {
                        return Self::render_virtualized_markdown(
                            message_id,
                            "message-markdown",
                            text,
                            window,
                            cx,
                        );
                    }
                    return Self::render_virtualized_plain_text(
                        message_id,
                        "message-plain",
                        text,
                        window,
                        cx,
                    );
                }

                Self::render_markdown_or_plain_text(
                    SharedString::from(format!("chat-md-{}", message_id)),
                    text.clone(),
                    window,
                    cx,
                )
            }
            MessageContent::ToolCall(tool_call) => {
                let is_read_tool = matches!(tool_call.kind, agent_client_protocol::ToolKind::Read);
                let is_execute_tool =
                    matches!(tool_call.kind, agent_client_protocol::ToolKind::Execute);
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

                let language_hint = language_from_tool_title(&tool_call.title);
                let mut content_blocks: Vec<AnyElement> = Vec::new();

                for (content_index, content) in tool_call.content.iter().enumerate() {
                    match content {
                        agent_client_protocol::ToolCallContent::Content(c) => {
                            if let agent_client_protocol::ContentBlock::Text(t) = &c.content {
                                let text = t.text.as_str();
                                let block_key = format!("tool-extra-{content_index}");
                                let element =
                                    if is_read_tool || looks_like_numbered_code_lines(text) {
                                        Self::render_virtualized_code_block(
                                            message_id,
                                            &block_key,
                                            language_hint,
                                            text,
                                            VirtualizedCodeRenderOptions {
                                                parse_numbered_lines: true,
                                                prefer_editor: true,
                                                inner_scroll: !is_read_tool,
                                            },
                                            window,
                                            cx,
                                        )
                                    } else if is_execute_tool || looks_like_terminal_output(text) {
                                        Self::render_virtualized_code_block(
                                            message_id,
                                            &block_key,
                                            "sh",
                                            text,
                                            VirtualizedCodeRenderOptions {
                                                parse_numbered_lines: false,
                                                prefer_editor: false,
                                                inner_scroll: true,
                                            },
                                            window,
                                            cx,
                                        )
                                    } else if should_virtualize_text(text) {
                                        if should_render_markdown(text) {
                                            Self::render_virtualized_markdown(
                                                message_id, &block_key, text, window, cx,
                                            )
                                        } else {
                                            Self::render_virtualized_plain_text(
                                                message_id, &block_key, text, window, cx,
                                            )
                                        }
                                    } else {
                                        Self::render_markdown_or_plain_text(
                                            SharedString::from(format!(
                                                "chat-tool-extra-{}-{}",
                                                message_id, content_index
                                            )),
                                            text.to_string(),
                                            window,
                                            cx,
                                        )
                                    };
                                content_blocks.push(element);
                            }
                        }
                        agent_client_protocol::ToolCallContent::Diff(d) => {
                            let path = d.path.display().to_string();
                            lines.push(format!("Diff: {path}"));
                            content_blocks.push(Self::render_virtualized_diff(
                                message_id,
                                &format!("diff-{path}"),
                                d.old_text.as_deref(),
                                &d.new_text,
                                window,
                                cx,
                            ));
                        }
                        agent_client_protocol::ToolCallContent::Terminal(terminal) => {
                            let terminal_id = terminal.terminal_id.to_string();
                            let transcript = app_state
                                .read(cx)
                                .terminal_transcript_for_thread(thread_id, &terminal_id)
                                .unwrap_or_default();
                            content_blocks.push(Self::render_terminal_widget(
                                message_id,
                                terminal_id,
                                transcript,
                                window,
                                cx,
                            ));
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
                    .children(content_blocks)
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
        if should_virtualize_text(&transcript) {
            return Self::render_virtualized_code_block(
                message_id,
                &format!("terminal-{terminal_id}"),
                "sh",
                &transcript,
                VirtualizedCodeRenderOptions {
                    parse_numbered_lines: false,
                    prefer_editor: false,
                    inner_scroll: true,
                },
                window,
                cx,
            );
        }
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
            thread_id,
            message_id,
            is_collapsible,
            is_expanded,
            collapsed_preview,
        ): (Rgba, Rgba, Uuid, Uuid, bool, bool, Option<String>) = {
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

            // For now, we only collapse Text messages.
            let is_expanded = expanded_messages.contains(&message.id);
            let mut is_collapsible = false;
            let mut collapsed_preview = None;

            if let MessageContent::Text(text) = &message.content
                && !text.contains("--- before\n+++ after")
            {
                let preview_candidate =
                    collapsed_text_preview(text, COLLAPSE_LINE_LIMIT, COLLAPSE_CHAR_LIMIT);
                is_collapsible = preview_candidate.is_some();
                if !is_expanded && let Some(preview) = preview_candidate {
                    collapsed_preview = Some(preview);
                }
            }

            (
                bg,
                text_color,
                message.thread_id,
                message.id,
                is_collapsible,
                is_expanded,
                collapsed_preview,
            )
        };

        let content_el = if let Some(preview) = collapsed_preview {
            Self::render_markdown_or_plain_text(
                SharedString::from(format!("chat-collapsed-preview-{message_id}")),
                preview,
                window,
                cx,
            )
        } else {
            let content_cache = window.use_keyed_state(
                SharedString::from(format!("chat-message-content-{message_id}")),
                cx,
                |_window, _cx| MessageContentCache::default(),
            );
            content_cache.update(cx, |cache, cx| {
                let state = app_state.read(cx);
                let Some(thread) = state.active_thread() else {
                    return;
                };
                let Some(message) = thread.messages.get(index) else {
                    return;
                };
                let signature = message_tail_signature(message);
                if cache.signature != Some(signature) {
                    cache.signature = Some(signature);
                    cache.content = Some(Arc::new(message.content.clone()));
                }
            });
            let Some(message_content) = content_cache.read(cx).content.clone() else {
                return div().into_any_element();
            };
            Self::render_message_content(
                app_state,
                thread_id,
                message_id,
                message_content.as_ref(),
                window,
                cx,
            )
        };

        let this_expand = this.clone();
        let app_state_for_copy = app_state.clone();
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
                let copied = app_state_for_copy
                    .read(cx)
                    .active_thread()
                    .and_then(|thread| {
                        thread
                            .messages
                            .iter()
                            .find(|message| message.id == message_id)
                    })
                    .map(|message| message.content.to_string());
                if let Some(copied) = copied {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied));
                }
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
        self.normalized_scroll_offset()
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
        if offset.y <= px(2.0) {
            self.list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            });
            self.locked_to_bottom = false;
        } else {
            self.list_state.set_offset_from_scrollbar(offset);
            let max_y = self.list_state.max_offset_for_scrollbar().height;
            self.locked_to_bottom = max_y > px(0.0) && (max_y - offset.y).abs() <= px(2.0);
        }
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

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_expand_message(&mut self, message_id: Uuid) {
        self.expanded_messages.insert(message_id);
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
            let max_y = self.list_state.max_offset_for_scrollbar().height;
            let current_y = self
                .list_state
                .scroll_px_offset_for_scrollbar()
                .y
                .abs()
                .min(max_y);
            if (max_y - current_y).abs() > px(2.0) {
                self.scroll_to_bottom();
            }
        }

        let (
            active_thread_id,
            message_count,
            tail_signature,
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
                state
                    .active_thread()
                    .and_then(|thread| thread.messages.last())
                    .map(message_tail_signature),
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
        let tail_changed = tail_signature != self.last_tail_signature;

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
                if message_count > 0 && tail_changed {
                    self.list_state.splice(message_count - 1..message_count, 1);
                }
            }
        }
        self.listed_count = total_count;
        self.last_tail_signature = tail_signature;

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
                    .flex()
                    .flex_1()
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .debug_selector(|| "chat-message-list-container".to_string())
                    .child(
                        div()
                            .flex_1()
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
                    .child(
                        Scrollbar::vertical(&self.list_state)
                            .scrollbar_show(ScrollbarShow::Always),
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

        let has_config_options = config_options
            .as_ref()
            .map(|opts| !opts.is_empty())
            .unwrap_or(false);

        // Build left-side option columns for the unified info row.
        let mut left_cols: Vec<AnyElement> = Vec::new();

        // Agent column: shown when configured agents exist.
        // - Thread unlocked: clickable selector buttons (one per configured agent).
        // - Thread locked: static label showing which agent is in use.
        if !configured_agents.is_empty()
            && let Some(thread_id) = active_thread_id
        {
            let agent_content: AnyElement = if is_agent_locked {
                let label = locked_agent
                    .as_deref()
                    .unwrap_or("unknown agent")
                    .to_string();
                div()
                    .text_xs()
                    .text_color(rgb(0xdddddd))
                    .child(label)
                    .into_any_element()
            } else {
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
            left_cols.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(0x888888)).child("Agent"))
                    .child(agent_content)
                    .into_any_element(),
            );
        }

        // Model column
        if let (
            Some(_thread_id),
            Some(SessionModelState {
                current_model_id,
                available_models,
                ..
            }),
            false,
        ) = (active_thread_id, models, has_config_options)
            && !available_models.is_empty()
        {
            let options = available_models
                .into_iter()
                .map(|model| PickerOption::new(model.model_id.to_string(), model.name))
                .collect::<Vec<_>>();
            let current_model_id = current_model_id.to_string();
            self.model_select_state.update(cx, |state, cx| {
                state.set_items(options, _window, cx);
                state.set_selected_value(&current_model_id, _window, cx);
            });
            left_cols.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(0x888888)).child("Model"))
                    .child(
                        div()
                            .id("chat-model-select")
                            .child(Select::new(&self.model_select_state).menu_width(px(260.0))),
                    )
                    .into_any_element(),
            );
        }

        // Mode column
        if let (
            Some(_thread_id),
            Some(SessionModeState {
                current_mode_id,
                available_modes,
                ..
            }),
            false,
        ) = (active_thread_id, modes, has_config_options)
            && !available_modes.is_empty()
        {
            let options = available_modes
                .into_iter()
                .map(|mode| PickerOption::new(mode.id.to_string(), mode.name))
                .collect::<Vec<_>>();
            let current_mode_id = current_mode_id.to_string();
            self.mode_select_state.update(cx, |state, cx| {
                state.set_items(options, _window, cx);
                state.set_selected_value(&current_mode_id, _window, cx);
            });
            left_cols.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(0x888888)).child("Mode"))
                    .child(
                        div()
                            .id("chat-mode-select")
                            .child(Select::new(&self.mode_select_state).menu_width(px(220.0))),
                    )
                    .into_any_element(),
            );
        }

        // Config option columns
        if let (Some(_thread_id), Some(options)) = (active_thread_id, config_options) {
            for (option_index, option) in options.into_iter().enumerate() {
                let _option_id = option.id.to_string();
                let label = div().text_xs().text_color(rgb(0x888888)).child(option.name);
                let control: AnyElement = match option.kind {
                    agent_client_protocol::SessionConfigKind::Select(select) => {
                        let entries = match select.options {
                            agent_client_protocol::SessionConfigSelectOptions::Ungrouped(
                                values,
                            ) => values
                                .into_iter()
                                .map(|entry| PickerOption::new(entry.value.to_string(), entry.name))
                                .collect::<Vec<_>>(),
                            agent_client_protocol::SessionConfigSelectOptions::Grouped(groups) => {
                                groups
                                    .into_iter()
                                    .flat_map(|group| {
                                        group.options.into_iter().map(move |entry| {
                                            PickerOption::new(
                                                entry.value.to_string(),
                                                format!("{} / {}", group.group, entry.name),
                                            )
                                        })
                                    })
                                    .collect::<Vec<_>>()
                            }
                            _ => Vec::new(),
                        };
                        let current_value = select.current_value.to_string();
                        if option_index < self.config_select_states.len() {
                            let select_state = &self.config_select_states[option_index];
                            select_state.update(cx, |state, cx| {
                                state.set_items(entries, _window, cx);
                                state.set_selected_value(&current_value, _window, cx);
                                cx.notify();
                            });
                            div()
                                .id(("session-config-select", option_index))
                                .child(
                                    Select::new(select_state)
                                        .menu_width(px(220.0))
                                        .max_h(px(300.0)),
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    }
                    _ => div().into_any_element(),
                };
                left_cols.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label)
                        .child(control)
                        .into_any_element(),
                );
            }
        }

        let usage_el = usage.map(|usage| {
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
                .flex()
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

        // Unified info row: options left-aligned, usage right-aligned, no background.
        let info_row = div()
            .w_full()
            .px_3()
            .py_1()
            .flex()
            .items_end()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_4()
                    .items_end()
                    .children(left_cols),
            )
            .when_some(usage_el, |this, el| this.child(el));

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
            .child(info_row)
    }
}

fn usage_progress_percent(usage: &UsageUpdate) -> f32 {
    if usage.size == 0 {
        return 0.0;
    }
    ((usage.used as f32 / usage.size as f32) * 100.0).clamp(0.0, 100.0)
}

fn message_tail_signature(message: &crate::domain::Message) -> MessageTailSignature {
    let content_size = match &message.content {
        MessageContent::Text(text) => text.len(),
        MessageContent::ToolCall(tool_call) => {
            let mut size = tool_call.title.len() + tool_call.content.len() + 16;
            for content in &tool_call.content {
                match content {
                    agent_client_protocol::ToolCallContent::Content(content_block) => {
                        if let agent_client_protocol::ContentBlock::Text(text) =
                            &content_block.content
                        {
                            size += text.text.len();
                        }
                    }
                    agent_client_protocol::ToolCallContent::Diff(diff) => {
                        size += diff.new_text.len();
                        if let Some(old_text) = diff.old_text.as_ref() {
                            size += old_text.len();
                        }
                    }
                    agent_client_protocol::ToolCallContent::Terminal(_) => size += 32,
                    _ => {}
                }
            }
            size
        }
    };

    MessageTailSignature {
        message_id: message.id,
        content_size,
        is_streaming: message.is_streaming,
    }
}

fn collapsed_text_preview(text: &str, max_lines: usize, max_chars: usize) -> Option<String> {
    let mut parts = text.split('\n');
    let mut preview_lines = Vec::with_capacity(max_lines);
    for _ in 0..max_lines {
        let Some(part) = parts.next() else {
            break;
        };
        preview_lines.push(part);
    }
    if preview_lines.len() == max_lines && parts.next().is_some() {
        return Some(preview_lines.join("\n"));
    }
    if text.len() > max_chars {
        let mut end = max_chars.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut preview = text[..end].to_string();
        preview.push('…');
        return Some(preview);
    }
    None
}

fn should_render_markdown(text: &str) -> bool {
    if text.len() <= MARKDOWN_FASTPATH_LENGTH {
        return true;
    }
    text.contains("```")
        || text.contains('`')
        || text.contains("\n#")
        || text.contains("\n- ")
        || text.contains("\n* ")
        || text.contains("\n1. ")
        || text.contains("](")
        || text.contains("**")
        || text.contains("__")
        || text.contains("\n> ")
}

#[derive(Clone)]
struct CodeRenderLine {
    sign: char,
    line_number: Option<usize>,
    text: String,
    is_added: bool,
    is_removed: bool,
}

fn text_fingerprint(text: &str) -> TextFingerprint {
    let bytes = text.as_bytes();
    let mut head = 0u64;
    for byte in bytes.iter().take(32) {
        head = head
            .wrapping_mul(16_777_619)
            .wrapping_add((*byte as u64) + 1);
    }
    let mut mid = 0u64;
    if !bytes.is_empty() {
        let mid_start = bytes.len().saturating_sub(16) / 2;
        let mid_end = (mid_start + 32).min(bytes.len());
        for byte in &bytes[mid_start..mid_end] {
            mid = mid
                .wrapping_mul(2_166_136_261)
                .wrapping_add((*byte as u64) + 1);
        }
    }
    let mut tail = 0u64;
    for byte in bytes.iter().rev().take(32) {
        tail = tail
            .wrapping_mul(1_099_511_628_211)
            .wrapping_add((*byte as u64) + 1);
    }
    TextFingerprint {
        len: text.len(),
        head,
        mid,
        tail,
    }
}

fn should_virtualize_text(text: &str) -> bool {
    if text.len() >= VIRTUALIZE_CHAR_THRESHOLD {
        return true;
    }
    let mut lines = 1usize;
    for byte in text.as_bytes() {
        if *byte == b'\n' {
            lines += 1;
            if lines > VIRTUALIZE_LINE_THRESHOLD {
                return true;
            }
        }
    }
    false
}

fn build_text_chunks(text: &str) -> Vec<SharedString> {
    let mut chunks = Vec::new();
    let mut current = Vec::with_capacity(VIRTUALIZED_CHUNK_LINES);
    let push_line = |line: &str, chunks: &mut Vec<SharedString>, current: &mut Vec<String>| {
        current.push(line.to_string());
        if current.len() == VIRTUALIZED_CHUNK_LINES {
            chunks.push(SharedString::from(current.join("\n")));
            current.clear();
        }
    };

    if text.is_empty() {
        push_line("", &mut chunks, &mut current);
    } else {
        for line in text.split('\n') {
            push_line(line, &mut chunks, &mut current);
        }
    }

    if !current.is_empty() {
        chunks.push(SharedString::from(current.join("\n")));
    }
    chunks
}

fn parse_numbered_code_line(line: &str) -> Option<(usize, &str)> {
    if let Some((left, right)) = line.split_once('|')
        && let Ok(number) = left.trim().parse::<usize>()
    {
        return Some((number, right.strip_prefix(' ').unwrap_or(right)));
    }
    if let Some((left, right)) = line.split_once('.')
        && let Ok(number) = left.trim().parse::<usize>()
    {
        return Some((number, right.strip_prefix(' ').unwrap_or(right)));
    }
    let (left, right) = line.split_once(':')?;
    let number = left.trim().parse::<usize>().ok()?;
    Some((number, right.strip_prefix(' ').unwrap_or(right)))
}

fn push_virtualized_code_chunk(chunks: &mut Vec<VirtualizedCodeChunk>, lines: &[CodeRenderLine]) {
    if lines.is_empty() {
        return;
    }
    let mut code_text = String::new();
    let mut line_numbers = String::new();
    let mut signs = String::new();
    let mut has_added = false;
    let mut has_removed = false;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            code_text.push('\n');
            line_numbers.push('\n');
            signs.push('\n');
        }
        code_text.push_str(&line.text);
        if let Some(number) = line.line_number {
            let _ = write!(&mut line_numbers, "{number:>6}");
        }
        signs.push(line.sign);
        has_added |= line.is_added;
        has_removed |= line.is_removed;
    }
    chunks.push(VirtualizedCodeChunk {
        code_text,
        line_numbers,
        signs,
        line_count: lines.len(),
        has_added,
        has_removed,
    });
}

fn merge_virtualized_code_chunks(chunks: &[VirtualizedCodeChunk]) -> VirtualizedCodeChunk {
    let mut code_text = String::new();
    let mut line_numbers = String::new();
    let mut signs = String::new();
    let mut line_count = 0usize;
    let mut has_added = false;
    let mut has_removed = false;

    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            code_text.push('\n');
            line_numbers.push('\n');
            signs.push('\n');
        }
        code_text.push_str(&chunk.code_text);
        line_numbers.push_str(&chunk.line_numbers);
        signs.push_str(&chunk.signs);
        line_count += chunk.line_count;
        has_added |= chunk.has_added;
        has_removed |= chunk.has_removed;
    }

    VirtualizedCodeChunk {
        code_text,
        line_numbers,
        signs,
        line_count,
        has_added,
        has_removed,
    }
}

fn build_non_scrolling_render_chunks(chunks: &[VirtualizedCodeChunk]) -> Vec<VirtualizedCodeChunk> {
    let mut merged = Vec::new();
    let mut current = Vec::new();
    let mut current_line_count = 0usize;

    for chunk in chunks {
        if current_line_count > 0
            && current_line_count + chunk.line_count > NON_SCROLL_RENDER_CHUNK_LINES
        {
            merged.push(merge_virtualized_code_chunks(&current));
            current.clear();
            current_line_count = 0;
        }
        current.push(chunk.clone());
        current_line_count += chunk.line_count;
    }

    if !current.is_empty() {
        merged.push(merge_virtualized_code_chunks(&current));
    }
    merged
}

fn build_virtualized_code_chunks(
    text: &str,
    parse_numbered_lines: bool,
) -> Vec<VirtualizedCodeChunk> {
    let mut chunks = Vec::new();
    let mut current = Vec::with_capacity(VIRTUALIZED_CHUNK_LINES);
    let mut fallback_line = 1usize;
    let push_line = |raw: &str, lines: &mut Vec<CodeRenderLine>, fallback_line: &mut usize| {
        let (line_number, text) = if parse_numbered_lines {
            if let Some((number, text)) = parse_numbered_code_line(raw) {
                *fallback_line = number.saturating_add(1);
                (Some(number), text.to_string())
            } else {
                (Some(*fallback_line), raw.to_string())
            }
        } else {
            (Some(*fallback_line), raw.to_string())
        };
        lines.push(CodeRenderLine {
            sign: ' ',
            line_number,
            text,
            is_added: false,
            is_removed: false,
        });
        *fallback_line += 1;
    };

    if text.is_empty() {
        push_line("", &mut current, &mut fallback_line);
    } else {
        for line in text.split('\n') {
            push_line(line, &mut current, &mut fallback_line);
            if current.len() == VIRTUALIZED_CHUNK_LINES {
                push_virtualized_code_chunk(&mut chunks, &current);
                current.clear();
            }
        }
    }

    if !current.is_empty() {
        push_virtualized_code_chunk(&mut chunks, &current);
    }
    chunks
}

fn looks_like_numbered_code_lines(text: &str) -> bool {
    let numbered = text
        .lines()
        .take(8)
        .filter(|line| parse_numbered_code_line(line).is_some())
        .count();
    numbered >= 3
}

fn looks_like_terminal_output(text: &str) -> bool {
    text.lines().take(40).any(|line| {
        line.contains(" ... ")
            || line.contains("FAILED")
            || line.contains("error:")
            || line.starts_with('$')
            || line.starts_with('>')
    })
}

fn parse_diff_hunk_header(line: &str) -> Option<(usize, usize)> {
    if !line.starts_with("@@") {
        return None;
    }
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    if first != "@@" {
        return None;
    }
    let old_part = parts.next()?;
    let new_part = parts.next()?;
    let old_start = old_part
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse::<usize>()
        .ok()?;
    let new_start = new_part
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse::<usize>()
        .ok()?;
    Some((old_start, new_start))
}

fn build_diff_blocks(rows: Vec<VirtualizedDiffRow>) -> Arc<[VirtualizedDiffBlock]> {
    let mut blocks = Vec::new();
    let mut current_kind = None;
    let mut signs = String::new();
    let mut line_numbers = String::new();
    let mut text = String::new();

    let flush = |blocks: &mut Vec<VirtualizedDiffBlock>,
                 current_kind: &mut Option<DiffRowKind>,
                 signs: &mut String,
                 line_numbers: &mut String,
                 text: &mut String| {
        let Some(kind) = *current_kind else {
            return;
        };
        blocks.push(VirtualizedDiffBlock {
            kind,
            signs: SharedString::from(std::mem::take(signs)),
            line_numbers: SharedString::from(std::mem::take(line_numbers)),
            text: SharedString::from(std::mem::take(text)),
        });
        *current_kind = None;
    };

    for row in rows {
        if current_kind != Some(row.kind) {
            flush(
                &mut blocks,
                &mut current_kind,
                &mut signs,
                &mut line_numbers,
                &mut text,
            );
            current_kind = Some(row.kind);
        }

        if !signs.is_empty() {
            signs.push('\n');
            line_numbers.push('\n');
            text.push('\n');
        }
        signs.push(row.sign);
        line_numbers.push_str(row.line_number_label.as_ref());
        if row.text.is_empty() {
            text.push(' ');
        } else {
            text.push_str(row.text.as_ref());
        }
    }

    flush(
        &mut blocks,
        &mut current_kind,
        &mut signs,
        &mut line_numbers,
        &mut text,
    );

    Arc::from(blocks)
}

fn build_virtualized_diff_chunks(diff_text: &str) -> Vec<VirtualizedDiffChunk> {
    let mut rows = Vec::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut in_hunk = false;
    let push_row = |kind: DiffRowKind,
                    sign: char,
                    line_number: Option<usize>,
                    text: &str|
     -> VirtualizedDiffRow {
        let line_number_label = line_number
            .map(|line| SharedString::from(line.to_string()))
            .unwrap_or_default();
        VirtualizedDiffRow {
            kind,
            sign,
            line_number_label,
            text: SharedString::from(text.to_string()),
        }
    };

    let mut process_line = |line: &str| {
        if let Some((old_start, new_start)) = parse_diff_hunk_header(line) {
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            return;
        }
        if !in_hunk {
            return;
        }
        if line.starts_with('\\') {
            return;
        }
        if let Some(text) = line.strip_prefix('+') {
            rows.push(push_row(DiffRowKind::Added, '+', Some(new_line), text));
            new_line += 1;
            return;
        }
        if let Some(text) = line.strip_prefix('-') {
            rows.push(push_row(DiffRowKind::Removed, '-', Some(old_line), text));
            old_line += 1;
            return;
        }
        let text = line.strip_prefix(' ').unwrap_or(line);
        rows.push(push_row(DiffRowKind::Context, ' ', Some(new_line), text));
        old_line += 1;
        new_line += 1;
    };

    if diff_text.is_empty() {
        process_line("");
    } else {
        for line in diff_text.split('\n') {
            process_line(line);
        }
    }
    if rows.is_empty() {
        rows.push(push_row(DiffRowKind::Context, ' ', None, "(no changes)"));
    }

    let mut chunks = Vec::new();
    let mut current = Vec::with_capacity(VIRTUALIZED_CHUNK_LINES);
    for row in rows {
        current.push(row);
        if current.len() == VIRTUALIZED_CHUNK_LINES {
            chunks.push(VirtualizedDiffChunk {
                blocks: build_diff_blocks(std::mem::take(&mut current)),
            });
        }
    }
    if !current.is_empty() {
        chunks.push(VirtualizedDiffChunk {
            blocks: build_diff_blocks(current),
        });
    }
    chunks
}

fn language_from_tool_title(title: &str) -> &'static str {
    for token in title.split_whitespace().rev() {
        let candidate = token.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ':'
            )
        });
        if candidate.contains('.') {
            return language_from_path(candidate);
        }
    }
    "text"
}

fn language_from_path(path: &str) -> &'static str {
    let normalized_path = path.split_once(':').map(|(base, _)| base).unwrap_or(path);
    let ext = normalized_path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "h" => "cpp",
        "sh" | "zsh" | "bash" => "bash",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "diff" | "patch" => "diff",
        _ => "text",
    }
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

    #[::core::prelude::v1::test]
    fn collapsed_text_preview_collapses_extra_lines() {
        let text = "line1\nline2\nline3";
        assert_eq!(
            collapsed_text_preview(text, 2, COLLAPSE_CHAR_LIMIT),
            Some("line1\nline2".to_string())
        );
    }

    #[::core::prelude::v1::test]
    fn collapsed_text_preview_collapses_long_single_line() {
        let text = "a".repeat(COLLAPSE_CHAR_LIMIT + 1);
        let preview = collapsed_text_preview(&text, COLLAPSE_LINE_LIMIT, COLLAPSE_CHAR_LIMIT)
            .expect("expected collapsed preview");
        assert!(preview.ends_with('…'));
    }

    #[::core::prelude::v1::test]
    fn long_plain_text_skips_markdown_rendering() {
        let text = "x".repeat(MARKDOWN_FASTPATH_LENGTH + 32);
        assert!(!should_render_markdown(&text));
        let markdown_like = format!("{}\n- item", "x".repeat(MARKDOWN_FASTPATH_LENGTH + 32));
        assert!(should_render_markdown(&markdown_like));
    }

    #[::core::prelude::v1::test]
    fn virtualization_threshold_triggers_for_many_lines() {
        let mut text = String::new();
        for _ in 0..(VIRTUALIZE_LINE_THRESHOLD + 4) {
            text.push_str("line\n");
        }
        assert!(should_virtualize_text(&text));
    }

    #[::core::prelude::v1::test]
    fn parse_numbered_code_lines_extracts_gutter_number() {
        let parsed = parse_numbered_code_line("   42 | fn answer() {}").expect("should parse");
        assert_eq!(parsed.0, 42);
        assert_eq!(parsed.1, "fn answer() {}");
        let parsed_colon =
            parse_numbered_code_line("  183: let answer = 42;").expect("should parse");
        assert_eq!(parsed_colon.0, 183);
        assert_eq!(parsed_colon.1, "let answer = 42;");
        let parsed_indent =
            parse_numbered_code_line("  184:     indented();").expect("should parse");
        assert_eq!(parsed_indent.0, 184);
        assert_eq!(parsed_indent.1, "    indented();");
        assert!(parse_numbered_code_line("not numbered").is_none());
    }

    #[::core::prelude::v1::test]
    fn code_chunk_builder_splits_large_payloads() {
        let mut payload = String::new();
        for index in 1..=400 {
            let _ = writeln!(payload, "{index:>6} | let value_{index} = {index};");
        }
        let chunks = build_virtualized_code_chunks(&payload, true);
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].line_count, VIRTUALIZED_CHUNK_LINES);
        assert!(chunks[0].line_numbers.contains("     1"));
    }

    #[::core::prelude::v1::test]
    fn code_chunk_builder_preserves_file_line_numbers_across_chunks() {
        let mut payload = String::new();
        for index in 183..=(183 + VIRTUALIZED_CHUNK_LINES + 1) {
            let _ = writeln!(payload, "{index:>6}: let value_{index} = {index};");
        }
        let chunks = build_virtualized_code_chunks(&payload, true);
        assert!(chunks.len() >= 2);
        assert!(
            chunks[0]
                .code_text
                .lines()
                .next()
                .expect("expected first line")
                .starts_with("let value_183")
        );
        assert_eq!(
            chunks[1]
                .line_numbers
                .lines()
                .next()
                .expect("expected first line number in second chunk"),
            format!("{:>6}", 183 + VIRTUALIZED_CHUNK_LINES)
        );
    }

    #[::core::prelude::v1::test]
    fn non_scrolling_render_chunk_builder_merges_small_chunks() {
        let mut payload = String::new();
        for index in 1..=450 {
            let _ = writeln!(payload, "{index:>6}: let value_{index} = {index};");
        }
        let base_chunks = build_virtualized_code_chunks(&payload, true);
        let render_chunks = build_non_scrolling_render_chunks(&base_chunks);
        assert!(base_chunks.len() > render_chunks.len());
        assert_eq!(render_chunks.iter().map(|chunk| chunk.line_count).sum::<usize>(), 451);
        assert!(
            render_chunks
                .iter()
                .all(|chunk| chunk.line_count <= NON_SCROLL_RENDER_CHUNK_LINES + 1)
        );
    }

    #[::core::prelude::v1::test]
    fn diff_chunk_builder_marks_added_and_removed_rows() {
        let diff = "\
--- before\n\
+++ after\n\
@@ -1,2 +1,2 @@\n\
-old line\n\
+new line\n\
 context line";
        let chunks = build_virtualized_diff_chunks(diff);
        assert!(!chunks.is_empty());
        let blocks = &chunks[0].blocks;
        assert!(
            blocks
                .iter()
                .any(|block| block.kind == DiffRowKind::Removed)
        );
        assert!(blocks.iter().any(|block| block.kind == DiffRowKind::Added));
    }

    #[::core::prelude::v1::test]
    fn diff_chunk_builder_skips_unified_diff_headers() {
        let diff = "\
--- before\n\
+++ after\n\
@@ -42,2 +42,2 @@\n\
-old line\n\
+new line";
        let chunks = build_virtualized_diff_chunks(diff);
        assert!(!chunks.is_empty());
        let rendered = chunks[0]
            .blocks
            .iter()
            .map(|block| block.text.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("@@ -42,2 +42,2 @@"));
        assert!(!rendered.contains("--- before"));
        assert!(!rendered.contains("+++ after"));
    }

    #[::core::prelude::v1::test]
    fn diff_chunk_builder_uses_file_relative_line_numbers() {
        let diff = "\
--- before\n\
+++ after\n\
@@ -183,1 +183,1 @@\n\
-removed content\n\
+added content";
        let chunks = build_virtualized_diff_chunks(diff);
        assert!(!chunks.is_empty());

        let removed = chunks[0]
            .blocks
            .iter()
            .find(|block| block.kind == DiffRowKind::Removed)
            .expect("expected removed block");
        let added = chunks[0]
            .blocks
            .iter()
            .find(|block| block.kind == DiffRowKind::Added)
            .expect("expected added block");

        assert_eq!(removed.line_numbers.as_ref(), "183");
        assert_eq!(removed.signs.as_ref(), "-");
        assert_eq!(removed.text.as_ref(), "removed content");

        assert_eq!(added.line_numbers.as_ref(), "183");
        assert_eq!(added.signs.as_ref(), "+");
        assert_eq!(added.text.as_ref(), "added content");
    }

    #[::core::prelude::v1::test]
    fn language_hint_prefers_extension_in_tool_title() {
        assert_eq!(language_from_tool_title("Read src/lib.rs"), "rust");
        assert_eq!(language_from_tool_title("Read src/lib.rs:183"), "rust");
        assert_eq!(language_from_tool_title("Read scripts/build.sh"), "bash");
        assert_eq!(language_from_tool_title("Read NOTES"), "text");
    }
}
