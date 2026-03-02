use crate::domain::Role;
use crate::state::AppState;
use gpui::prelude::*;
use gpui::*;

pub struct ChatView {
    app_state: Entity<AppState>,
    scroll_handle: UniformListScrollHandle,
    input_buffer: String,
    locked_to_bottom: bool,
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
            if event.keystroke.modifiers.control
                || event.keystroke.modifiers.alt
                || event.keystroke.modifiers.platform
            {
                return;
            }

            if event.keystroke.key == "backspace" {
                this.input_buffer.pop();
                cx.notify();
                return;
            }

            if let Some(input) = event.keystroke.key_char.as_deref() {
                if input == "\n" {
                    this.submit_input(cx);
                    return;
                }

                if input.chars().any(|ch| !ch.is_control()) {
                    this.input_buffer.push_str(input);
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
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_scroll_lock();

        let (active_thread_id, messages) = {
            let state = self.app_state.read(cx);
            (state.active_thread_id, state.active_thread_messages())
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

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(chat_content)
            .child(input_box)
    }
}
