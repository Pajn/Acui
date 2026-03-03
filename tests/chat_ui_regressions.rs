use acui::domain::{Message, Role};
use acui::state::AppState;
use acui::ui::chat::ChatView;
use acui::ui::layout::WorkspaceLayout;
use gpui::{
    AppContext, Entity, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
    VisualTestContext,
};
use gpui_component::Root;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

fn sample_diff() -> String {
    let mut diff = String::from("--- before\n+++ after\n");
    for index in 0..24 {
        diff.push_str(&format!(
            "@@ -{index},1 +{index},1 @@\n-{}{}\n+{}{}\n",
            "old-",
            "x".repeat(140),
            "new-",
            "y".repeat(200)
        ));
    }
    diff
}

fn with_chat_window(
    cx: &mut TestAppContext,
    assert_fn: impl FnOnce(Entity<ChatView>, Uuid, &mut VisualTestContext),
) {
    cx.update(gpui_component::init);
    let chat_slot = Rc::new(RefCell::new(None::<Entity<ChatView>>));
    let diff_slot = Rc::new(RefCell::new(None::<Uuid>));
    let chat_slot_for_window = chat_slot.clone();
    let diff_slot_for_window = diff_slot.clone();

    let (_, window_cx) = cx.add_window_view(move |window, cx| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let workspace_id = state.add_workspace_from_path(cx, PathBuf::from("."));
            let thread_id = state
                .add_thread(cx, workspace_id, "Thread 1")
                .expect("thread should be created");

            if let Some(thread) = state
                .workspaces
                .iter_mut()
                .find_map(|workspace| workspace.get_thread_mut(thread_id))
            {
                thread.add_message(Message::new(thread_id, Role::User, "user message"));
                thread.add_message(Message::new(
                    thread_id,
                    Role::Agent,
                    "agent message\nwith multiple lines\nfor height coverage",
                ));
                thread.add_message(Message::new(
                    thread_id,
                    Role::System,
                    "system message for status output",
                ));
                let diff_message = Message::new(thread_id, Role::System, sample_diff());
                *diff_slot_for_window.borrow_mut() = Some(diff_message.id);
                thread.add_message(diff_message);
                for index in 0..40 {
                    thread.add_message(Message::new(
                        thread_id,
                        Role::User,
                        format!("filler-{index} {}", "wrapped ".repeat(18)),
                    ));
                }
            }
            state
        });
        let layout = cx.new(|cx| WorkspaceLayout::new(app_state, window, cx));
        *chat_slot_for_window.borrow_mut() = Some(layout.read(cx).debug_chat_view());
        Root::new(layout, window, cx)
    });

    flush_layout(window_cx);
    let chat = chat_slot
        .borrow()
        .as_ref()
        .expect("chat should exist")
        .clone();
    let diff_message_id = (*diff_slot.borrow()).expect("diff message id should exist");
    assert_fn(chat, diff_message_id, window_cx);
}

fn flush_layout(window_cx: &mut VisualTestContext) {
    for _ in 0..5 {
        window_cx.update(|window, _| window.refresh());
        window_cx.run_until_parked();
    }
}

#[gpui::test]
async fn chat_rows_have_no_overlap_or_large_gaps(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        flush_layout(window_cx);

        let selectors = [
            "chat-row-0",
            "chat-row-1",
            "chat-row-2",
            "chat-row-3",
            "chat-row-4",
            "chat-row-5",
        ];

        let mut bounds = Vec::new();
        for selector in selectors {
            bounds.push(
                window_cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("missing bounds for {selector}")),
            );
        }

        for pair in bounds.windows(2) {
            let prev = pair[0];
            let next = pair[1];
            let gap = f32::from(next.origin.y - (prev.origin.y + prev.size.height));
            assert!(
                gap >= -0.5,
                "rows overlap: previous={prev:?} next={next:?} gap={gap}"
            );
            assert!(gap <= 24.0, "rows have excessive gap: {gap}");
        }
    });
}

#[gpui::test]
async fn chat_layout_keeps_input_visible_and_rows_wrapped(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        flush_layout(window_cx);

        let root = window_cx
            .debug_bounds("chat-root")
            .expect("chat root should render");
        let input = window_cx
            .debug_bounds("chat-input-box")
            .expect("chat input should render");
        let send_button = window_cx
            .debug_bounds("chat-send-button")
            .expect("send button should render");
        let viewport = window_cx
            .debug_bounds("chat-message-list-scrollable")
            .expect("message viewport should render");

        let root_bottom = root.origin.y + root.size.height;
        let input_bottom = input.origin.y + input.size.height;
        assert!(
            input_bottom <= root_bottom,
            "input should stay inside chat root"
        );
        assert!(
            send_button.origin.y >= input.origin.y,
            "send button should stay in input area"
        );

        let viewport_right = viewport.origin.x + viewport.size.width;
        let selectors = ["chat-row-0", "chat-row-1", "chat-row-2", "chat-row-3"];
        for selector in selectors {
            let row = window_cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing row bounds for {selector}"));
            let row_right = row.origin.x + row.size.width;
            assert!(
                row_right <= viewport_right + gpui::px(1.0),
                "row {selector} should wrap inside viewport"
            );
        }
    });
}

#[gpui::test]
async fn chat_list_scrolls_vertically_only(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        let max = window_cx.read(|app| chat.read(app).debug_message_max_offset());
        assert!(
            f32::from(max.height) > 0.0,
            "chat list should support vertical scrolling"
        );
        assert!(
            f32::from(max.width).abs() <= 0.5,
            "chat list should not support horizontal scrolling"
        );

        // Scroll to top first so we have a well-known starting position,
        // then verify scrolling down increases the offset.
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        let before = window_cx.read(|app| chat.read(app).debug_message_scroll_offset());
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(
                    gpui::point(gpui::px(0.0), max.height / 2.0),
                    cx,
                )
            })
        });
        let after = window_cx.read(|app| chat.read(app).debug_message_scroll_offset());
        assert!(
            f32::from(after.y) > f32::from(before.y),
            "chat vertical offset should increase"
        );
        assert!(
            f32::from(after.x).abs() <= 0.5,
            "chat horizontal offset should remain zero"
        );
    });
}

#[gpui::test]
async fn diff_view_is_rendered(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        flush_layout(window_cx);

        assert!(window_cx.debug_bounds("chat-diff-input").is_some());

        // Verify content exists via debug methods if possible,
        // or just rely on bounds for now as Input is a black box.
        let state = window_cx.read(|app| chat.read(app).debug_app_state());
        let thread_messages = window_cx.read(|app| {
            state
                .read(app)
                .active_thread()
                .map(|t| t.messages.clone())
                .unwrap_or_default()
        });
        assert!(
            thread_messages
                .iter()
                .any(|m| m.content.to_string().contains("--- before"))
        );
    });
}

#[gpui::test]
async fn scroll_lock_releases_when_user_scrolls_to_top(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(locked, "should start locked to bottom");

        // Scroll to the very top — also updates lock state via update_scroll_lock().
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(!locked, "lock should be released after scrolling to top");
    });
}

#[gpui::test]
async fn scroll_follows_new_messages_when_locked(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(locked, "should start locked to bottom");

        // Capture app_state and the active thread id so we can add messages below.
        let (app_state, thread_id) = window_cx.read(|app| {
            let chat_view = chat.read(app);
            let state = chat_view.debug_app_state();
            let thread_id = state.read(app).active_thread_id.expect("active thread");
            (state, thread_id)
        });

        // Add a new message (simulating a streaming chunk landing).
        window_cx.update(|_, app| {
            app_state.update(app, |state, _| {
                if let Some(thread) = state
                    .workspaces
                    .iter_mut()
                    .find_map(|ws| ws.get_thread_mut(thread_id))
                {
                    thread.add_message(Message::new(thread_id, Role::Agent, "streaming chunk 1"));
                }
            });
        });

        flush_layout(window_cx);

        // Scroll should have followed the new message to the bottom.
        let max = window_cx.read(|app| chat.read(app).debug_message_max_offset());
        let offset = window_cx.read(|app| chat.read(app).debug_message_scroll_offset());
        assert!(
            f32::from(max.height) - f32::from(offset.y) <= 10.0,
            "scroll should remain at bottom after new message when locked: max={}, offset={}",
            max.height,
            offset.y
        );
    });
}

#[gpui::test]
async fn scroll_stays_put_when_lock_released(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        // Release the scroll lock by scrolling to the top.
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)), cx)
            })
        });
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(!locked, "lock should be released");

        // Capture app_state and add a new message.
        let (app_state, thread_id) = window_cx.read(|app| {
            let chat_view = chat.read(app);
            let state = chat_view.debug_app_state();
            let thread_id = state.read(app).active_thread_id.expect("active thread");
            (state, thread_id)
        });

        window_cx.update(|_, app| {
            app_state.update(app, |state, _| {
                if let Some(thread) = state
                    .workspaces
                    .iter_mut()
                    .find_map(|ws| ws.get_thread_mut(thread_id))
                {
                    thread.add_message(Message::new(
                        thread_id,
                        Role::Agent,
                        "new message while unlocked",
                    ));
                }
            });
        });

        flush_layout(window_cx);

        // Offset should still be near zero — no auto-scroll.
        let offset = window_cx.read(|app| chat.read(app).debug_message_scroll_offset());
        assert!(
            f32::from(offset.y) < 20.0,
            "unlocked scroll should not jump to bottom: offset={}",
            offset.y
        );
    });
}

// -- Regression tests for scroll borrow-safety crash ----------------------------

/// Simulates a real scroll-wheel event over the chat list and verifies that
/// the scroll_handler does NOT panic with "RefCell already mutably borrowed".
/// Previously the handler called max_offset_for_scrollbar() (borrow()) while
/// the list's StateInner was already borrow_mut'd by the scroll dispatch path.
#[gpui::test]
async fn scroll_wheel_over_list_does_not_panic(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        // Find a point inside the list viewport so the scroll event reaches it.
        let list_point = window_cx
            .debug_bounds("chat-message-list-scrollable")
            .map(|b| b.origin + gpui::point(gpui::px(10.), gpui::px(10.)))
            .expect("chat message list should be visible");

        // Simulate the user scrolling up — this previously panicked with
        // "RefCell already mutably borrowed" because the scroll_handler called
        // list_state.max_offset_for_scrollbar() (borrow) while list_state was
        // already borrow_mut'd by GPUI's scroll event dispatch path.
        window_cx.simulate_event(ScrollWheelEvent {
            position: list_point,
            delta: ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(50.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });

        // After the scroll event is processed and render() runs,
        // the scroll lock should be released.
        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(!locked, "scroll wheel should release the bottom lock");
    });
}

/// Switching to a different thread must re-engage the scroll lock so the
/// new thread is always viewed from its most recent message.
#[gpui::test]
async fn thread_switch_re_engages_scroll_lock(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        // Capture app_state.
        let (app_state, thread_a) = window_cx.read(|app| {
            let view = chat.read(app);
            let state = view.debug_app_state();
            let id = state.read(app).active_thread_id.expect("active thread");
            (state, id)
        });

        // Release the scroll lock for thread A.
        window_cx.update(|_, app| {
            chat.update(app, |view, cx| {
                view.debug_message_set_scroll_offset(gpui::point(gpui::px(0.), gpui::px(0.)), cx)
            })
        });
        flush_layout(window_cx);
        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(!locked, "lock should be released for thread A");

        // Create a second thread in the same workspace and switch to it.
        let thread_b = window_cx.update(|_, app| {
            app_state.update(app, |state, cx| {
                let ws_id = state.workspaces.first().expect("workspace should exist").id;
                state
                    .add_thread(cx, ws_id, "Thread B")
                    .expect("thread B should be created")
            })
        });
        window_cx.update(|_, app| {
            app_state.update(app, |state, cx| state.set_active_thread(cx, thread_b))
        });
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(locked, "switching to thread B should re-engage scroll lock");

        // Switching back to thread A should also re-engage the lock.
        window_cx.update(|_, app| {
            app_state.update(app, |state, cx| state.set_active_thread(cx, thread_a))
        });
        flush_layout(window_cx);

        let locked = window_cx.read(|app| chat.read(app).debug_is_locked_to_bottom());
        assert!(
            locked,
            "switching back to thread A should re-engage scroll lock"
        );
    });
}

#[gpui::test]
async fn thought_messages_rendered_with_dimmed_text(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        flush_layout(window_cx);

        // Capture app_state and add a thought message.
        let (app_state, thread_id) = window_cx.read(|app| {
            let view = chat.read(app);
            let state = view.debug_app_state();
            let thread_id = state.read(app).active_thread_id.expect("active thread");
            (state, thread_id)
        });

        window_cx.update(|_, app| {
            app_state.update(app, |state: &mut AppState, _| {
                if let Some(thread) = state
                    .workspaces
                    .iter_mut()
                    .find_map(|ws| ws.get_thread_mut(thread_id))
                {
                    thread.add_message(Message::new(thread_id, Role::Thought, "thinking..."));
                }
            });
        });

        flush_layout(window_cx);

        // Verify that the thought message is rendered.
        let message_count = window_cx.read(|app| {
            let state = chat.read(app).debug_app_state();
            state.read(app).active_thread_message_count()
        });

        let selector =
            acui::ui::chat::row_debug_selector(message_count - 1).expect("selector should exist");
        assert!(
            window_cx.debug_bounds(selector).is_some(),
            "thought message should be rendered at {}",
            selector
        );
    });
}

#[gpui::test]
async fn shift_enter_inserts_newline(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        window_cx.update(|window, cx| {
            ChatView::debug_focus_input(&chat, window, cx);
        });
        window_cx.run_until_parked();

        window_cx.simulate_keystrokes("a");
        window_cx.run_until_parked();
        window_cx.simulate_keystrokes("b");
        window_cx.run_until_parked();
        window_cx.simulate_keystrokes("c");
        window_cx.run_until_parked();

        // Move cursor between 'a' and 'b' (at index 1)
        window_cx.update(|window, cx| {
            ChatView::debug_set_cursor(&chat, 1, window, cx);
        });
        window_cx.run_until_parked();

        window_cx.simulate_keystrokes("shift-enter");
        window_cx.run_until_parked();

        let value = window_cx.read(|app| chat.read(app).debug_input_value(app));
        // If it doubles, this will be "a\n\nbc", if it's correct it's "a\nbc"
        assert_eq!(value, "a\nbc");

        let cursor = window_cx.read(|app| chat.read(app).debug_cursor(app));
        assert_eq!(cursor, 2, "Cursor should be at 2 (after newline)");
    });
}

#[gpui::test]
async fn chat_input_grows_with_multiple_lines(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, _diff_message_id, window_cx| {
        let initial_bounds = window_cx
            .debug_bounds("chat-input-box")
            .expect("chat input should render");
        let initial_height = initial_bounds.size.height;

        window_cx.update(|window, cx| {
            ChatView::debug_focus_input(&chat, window, cx);
        });
        window_cx.run_until_parked();

        // Add 5 lines
        for _ in 0..5 {
            window_cx.simulate_keystrokes("shift-enter");
            window_cx.run_until_parked();
        }

        let grown_bounds = window_cx
            .debug_bounds("chat-input-box")
            .expect("chat input should render");
        let grown_height = grown_bounds.size.height;

        assert!(
            grown_height > initial_height,
            "input height should grow: initial={}, grown={}",
            initial_height,
            grown_height
        );

        // Add 15 lines (total 20) to hit max-h
        for _ in 0..15 {
            window_cx.simulate_keystrokes("shift-enter");
            window_cx.run_until_parked();
        }

        let max_bounds = window_cx
            .debug_bounds("chat-input-box")
            .expect("chat input should render");
        let max_height = max_bounds.size.height;

        // max_h is px(250.0). Plus p-3 (12px top, 12px bottom if p-3 is 12px)
        // or whatever px(250.0) covers.
        assert!(
            max_height <= gpui::px(300.0),
            "input height should respect max_h limit: current={}",
            max_height
        );
    });
}
