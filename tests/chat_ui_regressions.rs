use acui::domain::{Message, Role};
use acui::state::AppState;
use acui::ui::chat::ChatView;
use acui::ui::layout::WorkspaceLayout;
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
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
    with_chat_window(cx, |_chat, _diff_message_id, window_cx| {
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
    with_chat_window(cx, |_chat, _diff_message_id, window_cx| {
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

        let before = window_cx.read(|app| chat.read(app).debug_message_scroll_offset());
        window_cx.read(|app| {
            chat.read(app)
                .debug_message_set_scroll_offset(gpui::point(gpui::px(0.0), max.height / 2.0))
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
async fn diff_view_scrolls_in_both_axes(cx: &mut TestAppContext) {
    with_chat_window(cx, |chat, diff_message_id, window_cx| {
        flush_layout(window_cx);

        assert!(window_cx.debug_bounds("chat-diff-scrollable").is_some());
        assert!(window_cx.debug_bounds("chat-diff-scrollbar-v").is_some());
        assert!(window_cx.debug_bounds("chat-diff-scrollbar-h").is_some());

        let line0 = window_cx
            .debug_bounds("chat-diff-line-0")
            .expect("diff line 0 should render");
        let line8 = window_cx
            .debug_bounds("chat-diff-line-8")
            .expect("diff line 8 should render");
        assert!(
            line8.origin.y > line0.origin.y,
            "diff lines should stack vertically"
        );

        let max = window_cx.read(|app| {
            chat.read(app)
                .debug_diff_max_offset(diff_message_id)
                .expect("diff max offset should exist")
        });
        assert!(
            f32::from(max.height) > 0.0,
            "diff view should support vertical scrolling"
        );
        assert!(
            f32::from(max.width) > 0.0,
            "diff view should support horizontal scrolling"
        );

        let before = window_cx.read(|app| {
            chat.read(app)
                .debug_diff_scroll_offset(diff_message_id)
                .expect("diff offset should exist")
        });
        window_cx.read(|app| {
            chat.read(app).debug_diff_set_scroll_offset(
                diff_message_id,
                gpui::point(max.width / 2.0, max.height / 2.0),
            )
        });
        let after = window_cx.read(|app| {
            chat.read(app)
                .debug_diff_scroll_offset(diff_message_id)
                .expect("diff offset should exist after scroll")
        });
        assert!(
            f32::from(after.y) > f32::from(before.y),
            "diff vertical offset should increase"
        );
        assert!(
            f32::from(after.x) > f32::from(before.x),
            "diff horizontal offset should increase"
        );
    });
}
