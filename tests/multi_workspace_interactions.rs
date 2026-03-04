use acui::config::{AgentConfig, AppConfig};
use acui::state::AppState;
use gpui::{AppContext, Entity, TestAppContext};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

fn make_agent_config(workspace: &PathBuf) -> AgentConfig {
    AgentConfig {
        name: "mock-agent".to_string(),
        command: env!("CARGO_BIN_EXE_acui_mock_agent").to_string(),
        args: vec![],
        cwd: Some(workspace.clone()),
    }
}

fn create_state_entity(
    cx: &mut TestAppContext,
    data_dir: PathBuf,
    agent: AgentConfig,
) -> Entity<AppState> {
    cx.update(|cx| {
        cx.new(|_| {
            AppState::new_with_config(AppConfig {
                data_dir,
                agents: vec![agent],
                enable_mock_agent: false,
                log_file: None,
            })
        })
    })
}

fn wait_for_state(
    cx: &mut TestAppContext,
    entity: &Entity<AppState>,
    predicate: impl Fn(&AppState) -> bool,
) {
    for _ in 0..500 {
        cx.run_until_parked();
        if cx.read(|app| predicate(entity.read(app))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for expected state");
}

#[gpui::test]
async fn multi_workspace_cwds_are_distinct(cx: &mut TestAppContext) {
    let temp_dir =
        std::env::temp_dir().join(format!("acui-debug-multi-ws-{}", uuid::Uuid::new_v4()));
    let workspace_a = temp_dir.join("workspace-a");
    let workspace_b = temp_dir.join("workspace-b");
    let data_dir = temp_dir.join("data");

    fs::create_dir_all(&workspace_a).expect("create workspace a");
    fs::create_dir_all(&workspace_b).expect("create workspace b");
    fs::create_dir_all(&data_dir).expect("create data dir");

    let workspace_a = workspace_a.canonicalize().unwrap();
    let workspace_b = workspace_b.canonicalize().unwrap();

    // Config uses Workspace A as base, but we'll add Workspace B later
    let agent = make_agent_config(&workspace_a);
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());

    // 1. Add Workspace A
    let wid_a = state.update(cx, |state, cx| {
        state.add_workspace_from_path(cx, workspace_a.clone())
    });

    // Wait for pre-connection A
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace_a.clone()))
    });

    let thread_a = state.update(cx, |state, cx| {
        state.add_thread(cx, wid_a, "Thread A").expect("thread a")
    });

    // 2. Add Workspace B
    let wid_b = state.update(cx, |state, cx| {
        state.add_workspace_from_path(cx, workspace_b.clone())
    });

    // Wait for pre-connection B
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace_b.clone()))
    });

    let thread_b = state.update(cx, |state, cx| {
        state.add_thread(cx, wid_b, "Thread B").expect("thread b")
    });

    // 3. Test Thread B (should be Workspace B CWD)
    wait_for_state(cx, &state, |state| {
        state.thread_connection_is_some(thread_b)
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_b, "cwd");
    });

    let expected_b = workspace_b.display().to_string();
    let mut found_b = false;
    for _ in 0..500 {
        cx.run_until_parked();
        let messages = state.read_with(cx, |state, _| state.thread_messages(thread_b));
        for m in messages {
            let content = m.content.to_string();
            if content.contains("cwd:") {
                println!("DEBUG: Thread B received: {}", content);
                if content.contains(&expected_b) {
                    found_b = true;
                    break;
                }
            }
        }
        if found_b {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(found_b, "Thread B should contain CWD: {}", expected_b);

    // 4. Test Thread A (should be Workspace A CWD)
    wait_for_state(cx, &state, |state| {
        state.thread_connection_is_some(thread_a)
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_a, "cwd");
    });

    let expected_a = workspace_a.display().to_string();
    let mut found_a = false;
    for _ in 0..500 {
        cx.run_until_parked();
        let messages = state.read_with(cx, |state, _| state.thread_messages(thread_a));
        for m in messages {
            let content = m.content.to_string();
            if content.contains("cwd:") {
                println!("DEBUG: Thread A received: {}", content);
                if content.contains(&expected_a) {
                    found_a = true;
                    break;
                }
            }
        }
        if found_a {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(found_a, "Thread A should contain CWD: {}", expected_a);

    let _ = fs::remove_dir_all(temp_dir);
}
