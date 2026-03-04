use acui::config::{AgentConfig, AppConfig};
use acui::state::AppState;
use agent_client_protocol::SessionConfigKind;
use gpui::{AppContext, Entity, TestAppContext};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

fn integration_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("integration test lock poisoned")
}

fn make_agent_config(workspace: &PathBuf) -> AgentConfig {
    make_agent_config_with_env(workspace, &[])
}

fn make_agent_config_with_env(workspace: &PathBuf, env: &[(&str, &str)]) -> AgentConfig {
    if env.is_empty() {
        return AgentConfig {
            name: "mock-agent".to_string(),
            command: env!("CARGO_BIN_EXE_acui_mock_agent").to_string(),
            args: vec![],
            cwd: Some(workspace.clone()),
        };
    }
    let env_assignments = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    AgentConfig {
        name: "mock-agent".to_string(),
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!(
                "{env_assignments} {}",
                env!("CARGO_BIN_EXE_acui_mock_agent")
            ),
        ],
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
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-gpui-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
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
            .any(|message| message.content.to_string().contains(&expected_cwd))
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
            .any(|message| message.content.to_string().contains("permission outcome:"))
    });

    let _ = fs::remove_dir_all(temp_dir);
}

#[gpui::test]
async fn persisted_thread_reconnect_uses_load_session(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-load-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
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

#[gpui::test]
async fn persisted_thread_reconnect_uses_resume_session_without_load(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-resume-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config_with_env(&workspace, &[("ACUI_MOCK_DISABLE_LOAD", "1")]);
    let state_a = create_state_entity(cx, data_dir.clone(), agent.clone());
    let thread_id = state_a.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread should be created");
        state.select_agent_for_thread(cx, tid, agent.name.clone());
        tid
    });
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
            .is_some_and(|value| value == "resumed")
    });

    let _ = fs::remove_dir_all(temp_dir);
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
async fn workspace_sync_lists_sessions_and_creates_threads(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir =
        std::env::temp_dir().join(format!("acui-mock-list-sessions-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let workspace_str = workspace.display().to_string();
    let agent = make_agent_config_with_env(
        &workspace,
        &[
            ("ACUI_MOCK_SEED_SESSION", "1"),
            ("ACUI_MOCK_SEED_SESSION_ID", "mock-seeded-session"),
            ("ACUI_MOCK_SEED_CWD", &workspace_str),
        ],
    );
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());
    let workspace_id = state.update(cx, |state, cx| {
        state.add_workspace_from_path(cx, workspace.clone())
    });

    wait_for_state(cx, &state, |state| {
        state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .is_some_and(|workspace| {
                workspace
                    .threads
                    .iter()
                    .any(|thread| thread.session_id.as_deref() == Some("mock-seeded-session"))
            })
    });

    let created_thread_agent = state.update(cx, |state, _| {
        state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| {
                workspace
                    .threads
                    .iter()
                    .find(|thread| thread.session_id.as_deref() == Some("mock-seeded-session"))
            })
            .and_then(|thread| thread.agent_name.clone())
    });
    assert_eq!(created_thread_agent.as_deref(), Some("mock-agent"));

    let _ = fs::remove_dir_all(temp_dir);
}

#[gpui::test]
async fn mock_agent_exposes_modes_and_plan_updates(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-plan-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
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

    // Wait for pre-connection to be ready
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace.clone()))
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
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_models()
            .is_some_and(|models| models.current_model_id.to_string() == "gpt-5")
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
        state.set_session_model(cx, thread_id, "gpt-5-mini".to_string());
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_models()
            .is_some_and(|models| models.current_model_id.to_string() == "gpt-5-mini")
    });
    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "usage");
    });
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_usage()
            .is_some_and(|usage| usage.used > 0 && usage.size > 0 && usage.cost.is_some())
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

#[gpui::test]
async fn fork_session_creates_new_thread(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-fork-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config(&workspace);
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());
    let (workspace_id, thread_id) = state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread should be created");
        state.select_agent_for_thread(cx, tid, agent.name.clone());
        (workspace_id, tid)
    });

    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "hello");
    });
    wait_for_state(cx, &state, |state| {
        state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.threads.iter())
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.session_id.clone())
            .is_some()
    });

    state.update(cx, |state, cx| {
        state.fork_thread(cx, thread_id);
    });

    wait_for_state(cx, &state, |state| {
        state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .is_some_and(|workspace| {
                workspace.threads.iter().any(|thread| {
                    thread
                        .session_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("mock-session-fork-"))
                })
            })
    });

    let active_thread_is_fork = state.update(cx, |state, _| {
        state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.threads.iter())
            .find(|thread| Some(thread.id) == state.active_thread_id)
            .and_then(|thread| thread.session_id.clone())
            .is_some_and(|id| id.starts_with("mock-session-fork-"))
    });
    assert!(active_thread_is_fork);

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that pre-connected agents are populated when a new thread is created.
/// The pre-connected agents should be available asynchronously after thread creation.
#[gpui::test]
async fn preconnected_agents_populated_on_thread_creation(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir =
        std::env::temp_dir().join(format!("acui-mock-preconnect-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config(&workspace);
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());

    // Create a new thread - this should trigger pre-connection of all agents
    let _thread_id = state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        state.add_thread(cx, workspace_id, "Thread 1")
    });

    // Wait for pre-connected agents to be populated (async background task)
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace.clone()))
    });

    // The thread should have config options available from the pre-connected agent
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_config_options()
            .map(|opts| !opts.is_empty())
            .unwrap_or(false)
    });

    let has_config = state.update(cx, |state, _| {
        state
            .active_thread_config_options()
            .is_some_and(|opts| !opts.is_empty())
    });
    assert!(
        has_config,
        "Thread should have config options from pre-connected agent"
    );

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that agent switching is instant - config options should be available immediately
/// after selecting a different agent (copied from pre-connected agent).
#[gpui::test]
async fn agent_switching_is_instant(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-switch-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent1 = AgentConfig {
        name: "agent-1".to_string(),
        command: env!("CARGO_BIN_EXE_acui_mock_agent").to_string(),
        args: vec![],
        cwd: Some(workspace.clone()),
    };

    let agent2 = AgentConfig {
        name: "agent-2".to_string(),
        command: env!("CARGO_BIN_EXE_acui_mock_agent").to_string(),
        args: vec![],
        cwd: Some(workspace.clone()),
    };

    let state = cx.update(|cx| {
        cx.new(|_| {
            AppState::new_with_config(AppConfig {
                data_dir,
                agents: vec![agent1.clone(), agent2],
                enable_mock_agent: false,
                log_file: None,
            })
        })
    });

    // Create a thread to trigger pre-connection logic and give us something to switch on
    state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());
        state.add_thread(cx, workspace_id, "Thread 1");
    });

    // Wait for pre-connected agents to be ready
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("agent-1", Some(workspace.clone()))
            && state.agent_is_preconnected("agent-2", Some(workspace.clone()))
    });

    // Config for agent-1 should already be available because it was the first agent
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_config_options()
            .is_some_and(|opts| !opts.is_empty())
    });

    // Switch to agent-2 - this should be instant (just copying from pre-connected)
    let switched = state.update(cx, |state, cx| {
        if let Some(thread_id) = state.active_thread_id {
            state.select_agent_for_thread(cx, thread_id, "agent-2".to_string());
            true
        } else {
            false
        }
    });
    assert!(switched);

    // Config should be available immediately after switching (synchronous copy)
    let has_config_after_switch = state.update(cx, |state, _| {
        state
            .active_thread_config_options()
            .is_some_and(|opts| !opts.is_empty())
    });
    assert!(
        has_config_after_switch,
        "Config should be available immediately after agent switch"
    );

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that messages are sent through the pre-connected agent connection.
#[gpui::test]
async fn message_uses_preconnected_agent(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-msg-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config(&workspace);
    let state = cx.update(|cx| {
        cx.new(|_| {
            AppState::new_with_config(AppConfig {
                data_dir,
                agents: vec![agent.clone()],
                enable_mock_agent: false,
                log_file: None,
            })
        })
    });

    let thread_id = state.update(cx, |state, cx| {
        let workspace_id = state.add_workspace_from_path(cx, workspace.clone());

        state
            .add_thread(cx, workspace_id, "Thread 1")
            .expect("thread")
    });

    // Wait for pre-connection to be ready
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace.clone()))
    });

    // Send a message - should use the pre-connected agent
    state.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "cwd");
    });

    // Verify the message was processed
    let expected_cwd = workspace.display().to_string();
    wait_for_state(cx, &state, |state| {
        state
            .active_thread_messages()
            .iter()
            .any(|msg| msg.content.to_string().contains(&expected_cwd))
    });

    let message_received = state.update(cx, |state, _| {
        state
            .active_thread_messages()
            .iter()
            .any(|msg| msg.content.to_string().contains(&expected_cwd))
    });
    assert!(
        message_received,
        "Message should be processed via pre-connected agent"
    );

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that pre-connected agents in different workspaces use their respective workspace CWD.
#[gpui::test]
async fn multi_workspace_preconnection_cwd(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir =
        std::env::temp_dir().join(format!("acui-mock-multi-ws-{}", uuid::Uuid::new_v4()));
    let workspace_a = temp_dir.join("workspace-a");
    let workspace_b = temp_dir.join("workspace-b");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace_a).expect("should create workspace a");
    fs::create_dir_all(&workspace_b).expect("should create workspace b");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace_a = workspace_a.canonicalize().unwrap_or(workspace_a);
    let workspace_b = workspace_b.canonicalize().unwrap_or(workspace_b);

    let agent = make_agent_config(&workspace_a); // base config, CWD will be overridden by add_thread
    let state = create_state_entity(cx, data_dir.clone(), agent.clone());

    // 1. Create Workspace A
    let _wid_a = state.update(cx, |state, cx| {
        state.add_workspace_from_path(cx, workspace_a.clone())
    });

    // Wait for pre-connection for Workspace A
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace_a.clone()))
    });

    let thread_a = state.update(cx, |state, cx| {
        state.add_thread(cx, _wid_a, "Thread A").expect("thread a")
    });

    // 2. Create Workspace B
    let _wid_b = state.update(cx, |state, cx| {
        state.add_workspace_from_path(cx, workspace_b.clone())
    });

    // Wait for pre-connection for Workspace B
    wait_for_state(cx, &state, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace_b.clone()))
    });

    let thread_b = state.update(cx, |state, cx| {
        state.add_thread(cx, _wid_b, "Thread B").expect("thread b")
    });

    // 3. Send "cwd" in Workspace B. It should return workspace_b path.
    wait_for_state(cx, &state, |state| {
        state.thread_connection_is_some(thread_b)
    });

    state.update(cx, |state, cx| {
        state.set_active_thread(cx, thread_b);
        state.send_user_message(cx, thread_b, "cwd");
    });

    let expected_cwd_b = workspace_b.display().to_string();
    wait_for_state(cx, &state, |state| {
        state
            .thread_messages(thread_b)
            .iter()
            .any(|m| m.content.to_string().contains(&expected_cwd_b))
    });

    let message_b = state.update(cx, |state, _| {
        state
            .thread_messages(thread_b)
            .iter()
            .any(|m| m.content.to_string().contains(&expected_cwd_b))
    });
    assert!(message_b, "Thread B should have Workspace B CWD");

    // 4. Send "cwd" in Workspace A. It should return workspace_a path.
    wait_for_state(cx, &state, |state| {
        state.thread_connection_is_some(thread_a)
    });

    state.update(cx, |state, cx| {
        state.set_active_thread(cx, thread_a);
        state.send_user_message(cx, thread_a, "cwd");
    });

    let expected_cwd_a = workspace_a.display().to_string();
    wait_for_state(cx, &state, |state| {
        state
            .thread_messages(thread_a)
            .iter()
            .any(|m| m.content.to_string().contains(&expected_cwd_a))
    });

    let message_a = state.update(cx, |state, _| {
        state
            .thread_messages(thread_a)
            .iter()
            .any(|m| m.content.to_string().contains(&expected_cwd_a))
    });
    assert!(message_a, "Thread A should have Workspace A CWD");

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that a persisted thread (with a session_id) is NOT overwritten by a pre-connected agent
/// when a message is sent after an app restart.
#[gpui::test]
async fn persisted_session_not_overwritten_by_preconnect(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir = std::env::temp_dir().join(format!("acui-mock-no-over-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config(&workspace);
    let config = AppConfig {
        data_dir: data_dir.clone(),
        agents: vec![agent.clone()],
        enable_mock_agent: false,
        log_file: None,
    };

    // 1. Start app and establish a session
    let state_a = cx.update(|cx| cx.new(|_| AppState::new_with_config(config.clone())));
    let thread_id = state_a.update(cx, |state, cx| {
        let wid = state.add_workspace_from_path(cx, workspace.clone());
        state.add_thread(cx, wid, "Thread 1").expect("thread")
    });

    state_a.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "hello");
    });

    wait_for_state(cx, &state_a, |state| {
        state.active_thread_messages().len() >= 2
            && state.active_thread().unwrap().session_id.is_some()
    });

    let original_session_id = state_a.update(cx, |state, _| {
        state.active_thread().unwrap().session_id.clone().unwrap()
    });

    drop(state_a);

    // 2. Restart app
    let state_b = cx.update(|cx| {
        cx.new(|cx| {
            let mut state = AppState::new_with_config(config);
            state.restore_persisted_state(cx).unwrap();
            state
        })
    });

    // Wait for pre-connection to be ready (for workspace root)
    wait_for_state(cx, &state_b, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace.clone()))
    });

    // Wait for session to be fully restored and connected
    wait_for_state(cx, &state_b, |state| {
        state
            .active_thread()
            .is_some_and(|t| t.session_id.is_some())
            && state.active_thread_modes().is_some()
    });

    // 3. Send a message. It should use the ALREADY PERSISTED session_id, not the pre-connected one.
    state_b.update(cx, |state, cx| {
        state.send_user_message(cx, thread_id, "continue");
    });

    wait_for_state(cx, &state_b, |state| {
        state.active_thread_messages().len() >= 4
    });

    let final_session_id = state_b.update(cx, |state, _| {
        state.active_thread().unwrap().session_id.clone().unwrap()
    });

    assert_eq!(
        original_session_id, final_session_id,
        "Session ID should be preserved and NOT overwritten by pre-connected agent"
    );

    let _ = fs::remove_dir_all(temp_dir);
}

/// Test that agent options (modes, models) are available IMMEDIATELY after a thread
/// is selected after an app restart (transient state populated from pre-connection).
#[gpui::test]
async fn transient_options_available_after_restart(cx: &mut TestAppContext) {
    let _guard = integration_test_guard();
    let temp_dir =
        std::env::temp_dir().join(format!("acui-mock-transient-{}", uuid::Uuid::new_v4()));
    let workspace = temp_dir.join("workspace");
    let data_dir = temp_dir.join("data");
    fs::create_dir_all(&workspace).expect("should create workspace");
    fs::create_dir_all(&data_dir).expect("should create data dir");

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let agent = make_agent_config(&workspace);
    let config = AppConfig {
        data_dir: data_dir.clone(),
        agents: vec![agent.clone()],
        enable_mock_agent: false,
        log_file: None,
    };

    // 1. Establish a thread locked to an agent
    let state_a = cx.update(|cx| cx.new(|_| AppState::new_with_config(config.clone())));
    let thread_id = state_a.update(cx, |state, cx| {
        let wid = state.add_workspace_from_path(cx, workspace.clone());
        let tid = state.add_thread(cx, wid, "Thread 1").expect("thread");
        state.send_user_message(cx, tid, "hello");
        tid
    });
    wait_for_state(cx, &state_a, |state| {
        state.active_thread_messages().len() >= 2
    });
    drop(state_a);

    // 2. Restart
    let state_b = cx.update(|cx| cx.new(|_| AppState::new_with_config(config.clone())));
    state_b.update(cx, |state, cx| state.restore_persisted_state(cx).unwrap());

    // Wait for pre-connection to finish (it will happen in background for workspace root)
    wait_for_state(cx, &state_b, |state| {
        state.agent_is_preconnected("mock-agent", Some(workspace.clone()))
    });

    // 3. Switch to the thread. Options should be available soon (asynchronously from pre-connection propagation).
    state_b.update(cx, |state, cx| {
        state.set_active_thread(cx, thread_id);
    });

    wait_for_state(cx, &state_b, |state| state.active_thread_modes().is_some());

    let has_modes = state_b.update(cx, |state, _| state.active_thread_modes().is_some());
    assert!(
        has_modes,
        "Thread should have modes available after selection due to pre-connection propagation"
    );

    let _ = fs::remove_dir_all(temp_dir);
}
