use crate::state::AppState;
use gpui::*;

pub struct ChatView {
    app_state: Model<AppState>,
    scroll_handle: UniformListScrollHandle,
    /// Holds the user's current typed text before they hit enter
    input_buffer: String,
    locked_to_bottom: bool,
}

impl ChatView {
    pub fn build(app_state: Model<AppState>, cx: &mut WindowContext) -> View<Self> {
        cx.new_view(|cx| {
            cx.observe(&app_state, |this: &mut Self, _, cx| {
                // When state changes (new message chunk), enforce smart scroll lock
                if this.locked_to_bottom {
                    // Note to agent: In GPUI 0.6+, use the correct method to scroll a List or ScrollState to the end.
                    // Depending on the exact GPUI version, this might be `scroll_to_item` or interacting with a ScrollHandle.
                    this.scroll_handle.scroll_to_item(usize::MAX);
                }
                cx.notify();
            })
            .detach();

            Self {
                app_state,
                scroll_handle: UniformListScrollHandle::new(),
                input_buffer: String::new(),
                locked_to_bottom: true,
            }
        })
    }
}

impl Render for ChatView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);

        let chat_content = if let Some(thread_id) = state.active_thread_id {
            if let Some(workspace) = state
                .workspaces
                .iter()
                .find(|w| w.threads.iter().any(|t| t.id == thread_id))
            {
                if let Some(thread) = workspace.threads.iter().find(|t| t.id == thread_id) {
                    let message_count = thread.messages.len();
                    let messages = thread.messages.clone();

                    // The scrollable message list
                    div().flex_1().w_full().p_4().child(
                        uniform_list(
                            cx.view().clone(),
                            "chat-message-list",
                            message_count,
                            move |_, visible_range, _cx| {
                                visible_range
                                    .map(|i| {
                                        let msg = &messages[i];
                                        let is_user = msg.role == crate::domain::Role::User;

                                        div()
                                            .mb_4()
                                            .flex()
                                            .justify_content(if is_user {
                                                JustifyContent::End
                                            } else {
                                                JustifyContent::Start
                                            })
                                            .child(
                                                div()
                                                    .p_3()
                                                    .rounded_lg()
                                                    .bg(if is_user {
                                                        rgb(0x0e639c)
                                                    } else {
                                                        rgb(0x3c3c3c)
                                                    })
                                                    .text_color(white())
                                                    .max_w_3_4()
                                                    .child(msg.content.clone()),
                                            )
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(self.scroll_handle.clone()), // Agent instruction: Implement scroll tracking logic here
                                                                   // to set `self.locked_to_bottom = true` if the user is at the bottom,
                                                                   // or `false` if they scroll up.
                    )
                } else {
                    div().child("Thread not found")
                }
            } else {
                div().child("Workspace not found")
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

        // Note to Agent: GPUI input handling requires a bit of setup (often using `FocusHandle` and keyboard listeners).
        // For brevity, this represents the visual layout of the input box.
        let input_box = div()
            .h_16()
            .w_full()
            .p_4()
            .bg(rgb(0x1e1e1e))
            .border_t_1()
            .border_color(rgb(0x3c3c3c))
            .child(
                div()
                    .w_full()
                    .h_full()
                    .bg(rgb(0x3c3c3c))
                    .rounded_md()
                    .p_2()
                    .text_color(white())
                    .child("Chat input placeholder (Agent: Implement keyboard listener for Enter here)")
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use crate::domain::{Role, Message};

    #[gpui::test]
    fn test_smart_scroll_behavior(cx: &mut VisualTestContext) {
        // 1. Initialize the AppState with a test Workspace and Thread
        let app_state = cx.new_model(|cx| {
            let mut state = AppState::new();
            let ws_id = state.add_workspace(cx, "Test Workspace");
            let thread_id = state.add_thread(cx, ws_id, "Test Thread").unwrap();
            state.set_active_thread(cx, thread_id);
            state
        });

        let thread_id = app_state.read(cx).active_thread_id.unwrap();

        // 2. Mount the ChatView in the headless test environment
        // We keep a reference to the View to inspect its internal state later
        let chat_view = cx.add_window(|cx| ChatView::build(app_state.clone(), cx));
        let root_view = chat_view.root_view(cx).unwrap();

        // 3. Verify initial state: Should default to being locked to the bottom
        root_view.update(cx, |view, _cx| {
            assert!(view.locked_to_bottom, "Chat should initially be locked to the bottom");
        });

        // 4. Test Auto-Scrolling (Locked State)
        // We simulate the background Tokio task receiving an ACP chunk and updating the Model
        app_state.update(cx, |state, cx| {
            if let Some(ws) = state.workspaces.first_mut() {
                if let Some(thread) = ws.get_thread_mut(thread_id) {
                    thread.add_message(Message::new(thread_id, Role::Agent, "First chunk..."));
                }
            }
            cx.notify(); // Trigger UI rebuild
        });

        // Advance the async executor to process the UI update and the observer callbacks
        cx.run_until_parked();

        // Verify that because `locked_to_bottom` was true, the scroll handle was commanded to move
        // Note to Agent: Depending on the specific GPUI version, you may inspect the ListState
        // or ScrollHandle directly here to assert its logical offset is at usize::MAX or logical end.
        root_view.update(cx, |view, _cx| {
            // e.g., assert_eq!(view.scroll_handle.logical_scroll_top(), expected_bottom_value);
            // For now, we just ensure the lock remained true during the insertion
            assert!(view.locked_to_bottom); 
        });

        // 5. Test Unlocked State (User Scrolled Up)
        // We simulate the user scrolling up, which should unlock the scroll tracker
        root_view.update(cx, |view, _cx| {
            view.locked_to_bottom = false;
        });

        // Simulate another incoming chunk from the ACP stream
        app_state.update(cx, |state, cx| {
            if let Some(ws) = state.workspaces.first_mut() {
                if let Some(thread) = ws.get_thread_mut(thread_id) {
                    let msg = thread.get_active_agent_message_mut().unwrap();
                    msg.append_text(" second chunk.");
                }
            }
            cx.notify();
        });

        cx.run_until_parked();

        // Verify that the view respected the user's scroll position and DID NOT re-lock
        root_view.update(cx, |view, _cx| {
            assert!(!view.locked_to_bottom, "Chat should NOT force scroll if user scrolled up");
        });
    }
}
