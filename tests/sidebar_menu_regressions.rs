use acui::state::AppState;
use acui::ui::layout::WorkspaceLayout;
use gpui::{
    AppContext, Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px,
};
use gpui_component::Root;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

fn with_sidebar_window(
    cx: &mut TestAppContext,
    assert_fn: impl FnOnce(Entity<AppState>, Uuid, Uuid, &mut VisualTestContext),
) {
    cx.update(gpui_component::init);
    let app_state_slot = Rc::new(RefCell::new(None::<Entity<AppState>>));
    let thread_a_slot = Rc::new(RefCell::new(None::<Uuid>));
    let thread_b_slot = Rc::new(RefCell::new(None::<Uuid>));
    let app_state_slot_for_window = app_state_slot.clone();
    let thread_a_slot_for_window = thread_a_slot.clone();
    let thread_b_slot_for_window = thread_b_slot.clone();

    let (_, window_cx) = cx.add_window_view(move |window, cx| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let workspace_id = state.add_workspace_from_path(cx, PathBuf::from("."));
            let thread_a = state
                .add_thread(cx, workspace_id, "Thread 1")
                .expect("thread 1 should be created");
            let thread_b = state
                .add_thread(cx, workspace_id, "Thread 2")
                .expect("thread 2 should be created");
            *thread_a_slot_for_window.borrow_mut() = Some(thread_a);
            *thread_b_slot_for_window.borrow_mut() = Some(thread_b);
            state
        });
        *app_state_slot_for_window.borrow_mut() = Some(app_state.clone());
        let layout = cx.new(|cx| WorkspaceLayout::new(app_state, window, cx));
        Root::new(layout, window, cx)
    });

    flush_layout(window_cx);
    let app_state = app_state_slot
        .borrow()
        .as_ref()
        .expect("app_state should exist")
        .clone();
    let thread_a = (*thread_a_slot.borrow()).expect("thread 1 id should exist");
    let thread_b = (*thread_b_slot.borrow()).expect("thread 2 id should exist");
    assert_fn(app_state, thread_a, thread_b, window_cx);
}

fn flush_layout(window_cx: &mut VisualTestContext) {
    for _ in 0..5 {
        window_cx.update(|window, _| window.refresh());
        window_cx.run_until_parked();
    }
}

#[gpui::test]
async fn thread_context_menu_mark_unread_action_works(cx: &mut TestAppContext) {
    with_sidebar_window(cx, |app_state, thread_a, _thread_b, window_cx| {
        let row_bounds = window_cx
            .debug_bounds("sidebar-thread-row-0")
            .expect("first sidebar thread row should exist");
        let click_point = row_bounds.origin + point(px(12.0), px(12.0));

        window_cx.simulate_mouse_down(click_point, MouseButton::Right, Modifiers::default());
        window_cx.simulate_mouse_up(click_point, MouseButton::Right, Modifiers::default());
        flush_layout(window_cx);

        window_cx.simulate_keystrokes("down down down enter");
        flush_layout(window_cx);

        let has_unread = window_cx.read(|app| app_state.read(app).thread_has_unread_stop(thread_a));
        assert!(
            has_unread,
            "mark unread action should mark thread as unread"
        );
    });
}

#[gpui::test]
async fn thread_context_menu_delete_action_works(cx: &mut TestAppContext) {
    with_sidebar_window(cx, |app_state, thread_a, thread_b, window_cx| {
        let row_bounds = window_cx
            .debug_bounds("sidebar-thread-row-0")
            .expect("first sidebar thread row should exist");
        let click_point = row_bounds.origin + point(px(12.0), px(12.0));

        window_cx.simulate_mouse_down(click_point, MouseButton::Right, Modifiers::default());
        window_cx.simulate_mouse_up(click_point, MouseButton::Right, Modifiers::default());
        flush_layout(window_cx);

        window_cx.simulate_keystrokes("down down enter");
        flush_layout(window_cx);

        let (thread_count, thread_a_exists, thread_b_exists) = window_cx.read(|app| {
            let state = app_state.read(app);
            let threads: Vec<Uuid> = state
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.threads.iter().map(|thread| thread.id))
                .collect();
            (
                threads.len(),
                threads.contains(&thread_a),
                threads.contains(&thread_b),
            )
        });
        assert_eq!(thread_count, 1, "delete action should remove one thread");
        assert!(
            !thread_a_exists,
            "delete action should remove the clicked thread"
        );
        assert!(thread_b_exists, "other thread should remain after delete");
    });
}
