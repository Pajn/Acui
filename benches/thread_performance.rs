use acui::client::{AgentEvent, TerminalEvent};
use acui::domain::{Message, MessageContent, Role, Thread, Workspace};
use acui::state::AppState;
use acui::ui::chat::ChatView;
use acui::ui::layout::WorkspaceLayout;
use agent_client_protocol::{
    ContentBlock, ContentChunk, Diff, SessionId, SessionNotification, SessionUpdate, Terminal,
    TerminalExitStatus, TerminalId, ToolCall, ToolCallContent, ToolCallStatus, ToolKind,
};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
use gpui_component::Root;
use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

const AGENT_EVENT_COUNT: usize = 20_000;
const LARGE_THREAD_MESSAGE_COUNT: usize = 6_000;
const MASSIVE_LINE_COUNT: usize = 20_000;
const MASSIVE_LINE_WIDTH: usize = 180;

#[derive(Clone, Copy)]
struct ScenarioThreadIds {
    baseline: Uuid,
    plain: Uuid,
    markdown: Uuid,
    read_tool: Uuid,
    diff_tool: Uuid,
    terminal_tool: Uuid,
    plain_message_id: Uuid,
    markdown_message_id: Uuid,
}

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

fn finalized_message(thread_id: Uuid, role: Role, content: impl Into<MessageContent>) -> Message {
    let mut message = Message::new(thread_id, role, content);
    message.finalize();
    message
}

fn with_thread_mut(state: &mut AppState, thread_id: Uuid, f: impl FnOnce(&mut Thread)) {
    let thread = state
        .workspaces
        .iter_mut()
        .find_map(|workspace| workspace.get_thread_mut(thread_id))
        .expect("thread should exist");
    f(thread);
}

fn build_long_line_payload(prefix: &str) -> String {
    let repeated = "x".repeat(MASSIVE_LINE_WIDTH);
    let mut payload = String::with_capacity((MASSIVE_LINE_WIDTH + 24) * MASSIVE_LINE_COUNT);
    for index in 0..MASSIVE_LINE_COUNT {
        let _ = writeln!(payload, "{prefix}-{index:05} {repeated}");
    }
    payload
}

fn build_markdown_payload() -> String {
    let repeated = "m".repeat(MASSIVE_LINE_WIDTH);
    let mut payload = String::with_capacity((MASSIVE_LINE_WIDTH + 72) * MASSIVE_LINE_COUNT);
    for index in 0..MASSIVE_LINE_COUNT {
        let _ = writeln!(
            payload,
            "- **item-{index:05}** `marker` [ref](https://example.com/{index}) {repeated}"
        );
    }
    payload
}

fn build_read_output_payload() -> String {
    let repeated = "r".repeat(MASSIVE_LINE_WIDTH);
    let mut payload = String::with_capacity((MASSIVE_LINE_WIDTH + 48) * MASSIVE_LINE_COUNT);
    for index in 0..MASSIVE_LINE_COUNT {
        let _ = writeln!(
            payload,
            "{:>6} | fn read_fixture_{index:05}() {{ let line = \"{repeated}\"; }}",
            index + 1
        );
    }
    payload
}

fn build_terminal_output_payload() -> String {
    let repeated = "t".repeat(MASSIVE_LINE_WIDTH);
    let mut payload = String::with_capacity((MASSIVE_LINE_WIDTH + 48) * MASSIVE_LINE_COUNT);
    for index in 0..MASSIVE_LINE_COUNT {
        let status = if index % 37 == 0 { "FAILED" } else { "ok" };
        let _ = writeln!(payload, "test_case_{index:05} ... {status} {repeated}");
    }
    payload
}

fn build_diff_text(prefix: &str) -> String {
    let repeated = "d".repeat(MASSIVE_LINE_WIDTH);
    let mut payload = String::with_capacity((MASSIVE_LINE_WIDTH + 32) * MASSIVE_LINE_COUNT);
    for index in 0..MASSIVE_LINE_COUNT {
        let _ = writeln!(payload, "{prefix}_line_{index:05} {repeated}");
    }
    payload
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

            with_thread_mut(&mut state, small_thread_id, |thread| {
                thread.add_message(finalized_message(small_thread_id, Role::User, "hello"));
                thread.add_message(finalized_message(small_thread_id, Role::Agent, "world"));
            });

            with_thread_mut(&mut state, large_thread_id, |thread| {
                for index in 0..LARGE_THREAD_MESSAGE_COUNT {
                    let role = if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Agent
                    };
                    thread.add_message(finalized_message(
                        large_thread_id,
                        role,
                        format!("message-{index} {}", "payload ".repeat(8)),
                    ));
                }
            });

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

fn bench_ui_massive_payloads(c: &mut Criterion) {
    let mut app_cx = TestAppContext::single();
    app_cx.update(gpui_component::init);

    let app_state_slot = Rc::new(RefCell::new(None::<Entity<AppState>>));
    let chat_slot = Rc::new(RefCell::new(None::<Entity<ChatView>>));
    let thread_ids_slot = Rc::new(RefCell::new(None::<ScenarioThreadIds>));
    let app_state_slot_for_window = app_state_slot.clone();
    let chat_slot_for_window = chat_slot.clone();
    let thread_ids_slot_for_window = thread_ids_slot.clone();

    let (_, window_cx) = app_cx.add_window_view(move |window, cx| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let workspace_id = state.add_workspace_from_path(cx, PathBuf::from("."));
            let baseline = state
                .add_thread(cx, workspace_id, "Small Baseline")
                .expect("baseline thread should be created");
            let plain = state
                .add_thread(cx, workspace_id, "Plain 20k")
                .expect("plain thread should be created");
            let markdown = state
                .add_thread(cx, workspace_id, "Markdown 20k")
                .expect("markdown thread should be created");
            let read_tool = state
                .add_thread(cx, workspace_id, "Read Tool 20k")
                .expect("read thread should be created");
            let diff_tool = state
                .add_thread(cx, workspace_id, "Diff Tool 20k")
                .expect("diff thread should be created");
            let terminal_tool = state
                .add_thread(cx, workspace_id, "Terminal Tool 20k")
                .expect("terminal thread should be created");
            let mut thread_ids = ScenarioThreadIds {
                baseline,
                plain,
                markdown,
                read_tool,
                diff_tool,
                terminal_tool,
                plain_message_id: Uuid::nil(),
                markdown_message_id: Uuid::nil(),
            };

            with_thread_mut(&mut state, thread_ids.baseline, |thread| {
                thread.add_message(finalized_message(
                    thread_ids.baseline,
                    Role::User,
                    "baseline message",
                ));
                thread.add_message(finalized_message(
                    thread_ids.baseline,
                    Role::Agent,
                    "baseline response",
                ));
            });

            let plain_thread_id = thread_ids.plain;
            with_thread_mut(&mut state, plain_thread_id, |thread| {
                let message = finalized_message(
                    plain_thread_id,
                    Role::Agent,
                    build_long_line_payload("plain"),
                );
                thread_ids.plain_message_id = message.id;
                thread.add_message(message);
            });

            let markdown_thread_id = thread_ids.markdown;
            with_thread_mut(&mut state, markdown_thread_id, |thread| {
                let message =
                    finalized_message(markdown_thread_id, Role::Agent, build_markdown_payload());
                thread_ids.markdown_message_id = message.id;
                thread.add_message(message);
            });

            with_thread_mut(&mut state, thread_ids.read_tool, |thread| {
                let tool_call = ToolCall::new("bench-read-tool", "Read src/huge_fixture.rs")
                    .kind(ToolKind::Read)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(build_read_output_payload())]);
                thread.add_message(finalized_message(
                    thread_ids.read_tool,
                    Role::Agent,
                    tool_call,
                ));
            });

            with_thread_mut(&mut state, thread_ids.diff_tool, |thread| {
                let old_text = build_diff_text("before");
                let new_text = build_diff_text("after");
                let tool_call = ToolCall::new("bench-diff-tool", "Write src/huge_fixture.rs")
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Diff(
                        Diff::new("src/huge_fixture.rs", new_text).old_text(old_text),
                    )]);
                thread.add_message(finalized_message(
                    thread_ids.diff_tool,
                    Role::Agent,
                    tool_call,
                ));
            });

            let terminal_id = TerminalId::new("bench-terminal");
            state.apply_agent_event(
                thread_ids.terminal_tool,
                AgentEvent::Terminal(TerminalEvent::Started {
                    terminal_id: terminal_id.clone(),
                    command: "cargo test --all-features".to_string(),
                }),
            );
            state.apply_agent_event(
                thread_ids.terminal_tool,
                AgentEvent::Terminal(TerminalEvent::Output {
                    terminal_id: terminal_id.clone(),
                    chunk: build_terminal_output_payload(),
                }),
            );
            state.apply_agent_event(
                thread_ids.terminal_tool,
                AgentEvent::Terminal(TerminalEvent::Exited {
                    terminal_id: terminal_id.clone(),
                    exit_status: TerminalExitStatus::new().exit_code(1),
                }),
            );
            with_thread_mut(&mut state, thread_ids.terminal_tool, |thread| {
                let tool_call = ToolCall::new("bench-terminal-tool", "Run huge test suite")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Terminal(Terminal::new(
                        terminal_id.clone(),
                    ))]);
                thread.add_message(finalized_message(
                    thread_ids.terminal_tool,
                    Role::Agent,
                    tool_call,
                ));
            });

            state.active_thread_id = Some(thread_ids.baseline);
            *thread_ids_slot_for_window.borrow_mut() = Some(thread_ids);
            state
        });
        *app_state_slot_for_window.borrow_mut() = Some(app_state.clone());
        let layout = cx.new(|cx| WorkspaceLayout::new(app_state.clone(), window, cx));
        *chat_slot_for_window.borrow_mut() = Some(layout.read(cx).debug_chat_view());
        Root::new(layout, window, cx)
    });

    let app_state = app_state_slot
        .borrow()
        .as_ref()
        .expect("app state should be available")
        .clone();
    let chat = chat_slot
        .borrow()
        .as_ref()
        .expect("chat view should be available")
        .clone();
    let thread_ids = (*thread_ids_slot.borrow()).expect("scenario threads should be available");
    flush_layout(window_cx);
    window_cx.update(|_, app| {
        chat.update(app, |view, _| {
            view.debug_expand_message(thread_ids.plain_message_id);
            view.debug_expand_message(thread_ids.markdown_message_id);
        });
    });
    flush_layout(window_cx);

    let scenarios = [
        ("plain_message", thread_ids.plain),
        ("markdown_message", thread_ids.markdown),
        ("read_tool_call", thread_ids.read_tool),
        ("diff_tool_call", thread_ids.diff_tool),
        ("terminal_tool_call", thread_ids.terminal_tool),
    ];

    for (label, scenario_thread_id) in scenarios {
        let bench_name = format!("ui_open_20k_lines_{label}");
        c.bench_function(&bench_name, |b| {
            b.iter(|| {
                window_cx.update(|_, app| {
                    app_state.update(app, |state, cx| {
                        state.set_active_thread(cx, scenario_thread_id);
                    });
                });
                flush_layout(window_cx);
                black_box(window_cx.debug_bounds("chat-message-list-scrollable"));

                window_cx.update(|_, app| {
                    app_state.update(app, |state, cx| {
                        state.set_active_thread(cx, thread_ids.baseline);
                    });
                });
                flush_layout(window_cx);
            });
        });
    }

    for (label, scenario_thread_id) in scenarios {
        window_cx.update(|_, app| {
            app_state.update(app, |state, cx| {
                state.set_active_thread(cx, scenario_thread_id);
            });
        });
        flush_layout(window_cx);

        let mut scroll_to_bottom = false;
        let bench_name = format!("ui_scroll_20k_lines_{label}_step");
        c.bench_function(&bench_name, |b| {
            b.iter(|| {
                let max = window_cx.read(|app| chat.read(app).debug_message_max_offset().height);
                let target = if scroll_to_bottom {
                    max - (max / 8.0)
                } else {
                    max / 8.0
                };
                scroll_to_bottom = !scroll_to_bottom;

                window_cx.update(|_, app| {
                    chat.update(app, |view, cx| {
                        view.debug_message_set_scroll_offset(
                            gpui::point(gpui::px(0.0), target),
                            cx,
                        );
                    });
                });
                flush_layout(window_cx);
                black_box(window_cx.read(|app| chat.read(app).debug_message_scroll_offset()));
            });
        });

        window_cx.update(|_, app| {
            app_state.update(app, |state, cx| {
                state.set_active_thread(cx, thread_ids.baseline);
            });
        });
        flush_layout(window_cx);
    }
}

fn benches(c: &mut Criterion) {
    bench_agent_event_ingestion(c);
    bench_ui_large_thread_open(c);
    bench_ui_massive_payloads(c);
}

criterion_group!(thread_performance, benches);
criterion_main!(thread_performance);
