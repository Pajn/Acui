use acui::client::AgentEvent;
use acui::domain::{Message, Role, Thread, Workspace};
use acui::state::AppState;
use acui::ui::layout::WorkspaceLayout;
use agent_client_protocol::{
    ContentBlock, ContentChunk, SessionId, SessionNotification, SessionUpdate,
};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
use gpui_component::Root;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

const AGENT_EVENT_COUNT: usize = 20_000;
const LARGE_THREAD_MESSAGE_COUNT: usize = 6_000;

fn build_agent_event_notifications() -> Vec<SessionNotification> {
    (0..AGENT_EVENT_COUNT)
        .map(|index| {
            SessionNotification::new(
                SessionId::new("bench-session"),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(format!(
                    "chunk-{index} {}",
                    "payload ".repeat(4)
                )))),
            )
        })
        .collect()
}

fn build_state_for_agent_events() -> (AppState, Uuid) {
    let mut state = AppState::new();
    let workspace_id = Uuid::new_v4();
    let thread = Thread::new(workspace_id, "Bench Thread");
    let thread_id = thread.id;
    let mut workspace = Workspace::new("Bench Workspace");
    workspace.id = workspace_id;
    workspace.add_thread(thread);
    state.workspaces.push(workspace);
    state.active_thread_id = Some(thread_id);
    (state, thread_id)
}

fn flush_layout(window_cx: &mut VisualTestContext) {
    for _ in 0..3 {
        window_cx.update(|window, _| window.refresh());
        window_cx.run_until_parked();
    }
}

fn bench_agent_event_ingestion(c: &mut Criterion) {
    let notifications = build_agent_event_notifications();
    c.bench_function("agent_event_ingestion_20k_chunks", |b| {
        b.iter_batched(
            build_state_for_agent_events,
            |(mut state, thread_id)| {
                for notification in notifications.iter().cloned() {
                    state.apply_agent_event(thread_id, AgentEvent::Notification(notification));
                }
                state.apply_agent_event(thread_id, AgentEvent::Disconnected);
                black_box(state.active_thread_message_count());
            },
            BatchSize::LargeInput,
        )
    });
}

fn bench_ui_large_thread_open(c: &mut Criterion) {
    let mut app_cx = TestAppContext::single();
    app_cx.update(gpui_component::init);

    let app_state_slot = Rc::new(RefCell::new(None::<Entity<AppState>>));
    let small_thread_slot = Rc::new(RefCell::new(None::<Uuid>));
    let large_thread_slot = Rc::new(RefCell::new(None::<Uuid>));
    let app_state_slot_for_window = app_state_slot.clone();
    let small_thread_slot_for_window = small_thread_slot.clone();
    let large_thread_slot_for_window = large_thread_slot.clone();

    let (_, window_cx) = app_cx.add_window_view(move |window, cx| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let workspace_id = state.add_workspace_from_path(cx, PathBuf::from("."));
            let small_thread_id = state
                .add_thread(cx, workspace_id, "Small Thread")
                .expect("small thread should be created");
            let large_thread_id = state
                .add_thread(cx, workspace_id, "Large Thread")
                .expect("large thread should be created");

            if let Some(thread) = state
                .workspaces
                .iter_mut()
                .find_map(|workspace| workspace.get_thread_mut(small_thread_id))
            {
                thread.add_message(Message::new(small_thread_id, Role::User, "hello"));
                thread.add_message(Message::new(small_thread_id, Role::Agent, "world"));
            }

            if let Some(thread) = state
                .workspaces
                .iter_mut()
                .find_map(|workspace| workspace.get_thread_mut(large_thread_id))
            {
                for index in 0..LARGE_THREAD_MESSAGE_COUNT {
                    let role = if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Agent
                    };
                    thread.add_message(Message::new(
                        large_thread_id,
                        role,
                        format!("message-{index} {}", "payload ".repeat(8)),
                    ));
                }
            }

            state.active_thread_id = Some(small_thread_id);
            *small_thread_slot_for_window.borrow_mut() = Some(small_thread_id);
            *large_thread_slot_for_window.borrow_mut() = Some(large_thread_id);
            state
        });
        *app_state_slot_for_window.borrow_mut() = Some(app_state.clone());
        let layout = cx.new(|cx| WorkspaceLayout::new(app_state, window, cx));
        Root::new(layout, window, cx)
    });

    let app_state = app_state_slot
        .borrow()
        .as_ref()
        .expect("app state should be available")
        .clone();
    let small_thread_id = (*small_thread_slot.borrow()).expect("small thread should be set");
    let large_thread_id = (*large_thread_slot.borrow()).expect("large thread should be set");
    flush_layout(window_cx);

    c.bench_function("ui_open_large_thread_6k_messages", |b| {
        b.iter(|| {
            window_cx.update(|_, app| {
                app_state.update(app, |state, cx| {
                    state.set_active_thread(cx, large_thread_id);
                });
            });
            flush_layout(window_cx);
            black_box(window_cx.debug_bounds("chat-message-list-scrollable"));

            window_cx.update(|_, app| {
                app_state.update(app, |state, cx| {
                    state.set_active_thread(cx, small_thread_id);
                });
            });
            flush_layout(window_cx);
        });
    });
}

fn benches(c: &mut Criterion) {
    bench_agent_event_ingestion(c);
    bench_ui_large_thread_open(c);
}

criterion_group!(thread_performance, benches);
criterion_main!(thread_performance);
