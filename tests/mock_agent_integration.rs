use acui::config::{AgentConfig, AppConfig};
use acui::state::AppState;
use agent_client_protocol::SessionConfigKind;
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

#[gpui::test]
async fn mock_agent_subprocess_handles_cwd_and_permission(cx: &mut TestAppContext) {
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-gpui-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let agent = make_agent_config(&workspace);
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());
    let thread_id = state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread should be created");
        state.select_agent_for_thread(cx, tid, agent.name.clone());
        tid
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "cwd");
    });
    let expected_cwd = workspace.display().to_string();
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_messages()
            .iter()
            .any(|message| message.content.contains(&expected_cwd))
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "permission");
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_permission_options()
            .is_some_and(|options| !options.is_empty())
    });

    let selected_option = state.update(cx, |state, _| {
        state
            .active_thread_permission_options()
            .expect("permission options should be available")[0]
            .option_id
            .to_string()
    });
    state.update(cx, |state, cx| {
        state.resolve_permission(cx, thread_id, Some(selected_option));
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_messages()
            .iter()
            .any(|message| message.content.contains("permission outcome:"))
    });

    let _ = fs::remove_dir_all(temp_dir);
}

#[gpui::test]
async fn persisted_thread_reconnect_uses_load_session(cx: &mut TestAppContext) {
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-load-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let agent = make_agent_config(&workspace);
    let state_a = create_state_entity(cx, data_dir.clone(), agent.clone());
    let thread_id = state_a.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread should be created");
        state.select_agent_for_thread(cx, tid, agent.name.clone());
        tid
    });
    // Send a first message so the agent connects and a session_id is established.
    state_a.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "hello");
    });
    wait_for_state(cx, &state_a, |state| {
        state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.threads.iter())
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.session_id.clone())
            .is_some()
    });
    drop(state_a);

    let state_b = cx.update(|cx| {
        cx.new(|cx| {
            let mut state = AppState::new_with_config(AppConfig {
                data_dir: data_dir.clone(),
                agents: vec![agent],
                enable_mock_agent: false,
                log_file: None,
            });
            state
                .restore_persisted_state(cx)
                .expect("state restore should succeed");
            let active_thread = state.active_thread_id.expect("thread should restore");
            state.set_active_thread(cx, active_thread);
            state
        })
    });

    wait_for_state(cx, &state_b, |state| {
        state
            .active_thread_config_options()
            .and_then(|options: Vec<_>| {
                options.into_iter().find_map(|option| match option.kind {
                    SessionConfigKind::Select(select) if option.id.to_string() == "mode" => {
                        Some(select.current_value.to_string())
                    }
                    _ => None,
                })
            })
            .is_some_and(|value| value == "loaded")
    });

    let _ = fs::remove_dir_all(temp_dir);
}

fn wait_for_state(
    cx: &mut TestAppContext,
    entity: &Entity<AppState>,
    predicate: impl Fn(&AppState) -> bool,
) {
    for _ in 0..200 {
        cx.run_until_parked();
        if cx.read(|app| predicate(entity.read(app))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for expected state");
}

#[gpui::test]
async fn mock_agent_exposes_modes_and_plan_updates(cx: &mut TestAppContext) {
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-plan-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let state = create_state_entity(cx, data_dir.clone(), make_agent_config(&workspace));
    let thread_id = state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread should be created");
        let agent_name = state
            .configured_agents()
            .first()
            .map(|a| a.name.clone())
            .unwrap();
        state.select_agent_for_thread(cx, tid, agent_name);
        tid
    });

    // Send a first message so the agent connects and we receive the initial modes.
    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "hello");
    });

    wait_for_state(cx, &state, |state| {
        state
            .active_thread_modes()
            .is_some_and(|modes| modes.current_mode_id.to_string() == "ask")
    });

    state.update(cx, |state, cx| {
        state.set_session_mode(cx, thread_id, "code".to_string());
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_modes()
            .is_some_and(|modes| modes.current_mode_id.to_string() == "code")
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "plan");
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_plan()
            .is_some_and(|plan| !plan.entries.is_empty())
    });

    let _ = fs::remove_dir_all(temp_dir);
}
