use gpui::{Context, Task};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::{AcpController, AgentEvent, PermissionRequestEvent};
use crate::config::AppConfig;
use crate::domain::{Message, Role, Thread, Workspace};
use crate::persistence::AppPersistence;
use agent_client_protocol::{
    AvailableCommand, ContentBlock, ContentChunk, PermissionOption, PermissionOptionId, Plan,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionConfigId, SessionConfigOption,
    SessionConfigValueId, SessionId, SessionModeId, SessionModeState, SessionNotification,
    SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use ignore::WalkBuilder;
use similar::TextDiff;

struct ThreadAgentConnection {
    controller: Rc<AcpController>,
    session_id: SessionId,
    _process: Option<std::process::Child>,
}

/// All per-thread agent state in one place. A single `HashMap<Uuid, ThreadState>`
/// replaces the previous set of parallel HashMaps and ensures nothing is
/// accidentally left behind when a thread is deleted.
struct ThreadState {
    agent_task: Option<Task<()>>,
    mock_event_sender: Option<mpsc::UnboundedSender<AgentEvent>>,
    connection: Option<ThreadAgentConnection>,
    pending_permission: Option<PermissionRequestEvent>,
    config_options: Vec<SessionConfigOption>,
    available_commands: Vec<AvailableCommand>,
    tool_calls: HashMap<String, ToolCall>,
    tool_call_messages: HashMap<String, Uuid>,
    plan: Option<Plan>,
    mode: Option<SessionModeState>,
    prompt_started_at: Option<Instant>,
}

impl ThreadState {
    fn new() -> Self {
        Self {
            agent_task: None,
            mock_event_sender: None,
            connection: None,
            pending_permission: None,
            config_options: Vec::new(),
            available_commands: Vec::new(),
            tool_calls: HashMap::new(),
            tool_call_messages: HashMap::new(),
            plan: None,
            mode: None,
            prompt_started_at: None,
        }
    }
}

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_thread_id: Option<Uuid>,
    thread_state: HashMap<Uuid, ThreadState>,
    unread_stopped_threads: HashSet<Uuid>,
    log_file: Option<PathBuf>,
    persistence: Option<AppPersistence>,
    agent_config_path: Option<PathBuf>,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_parts(None, None, None)
    }

    pub fn new_with_config(config: AppConfig) -> Self {
        let AppConfig {
            data_dir,
            agent_config,
            log_file,
        } = config;
        Self::with_parts(Some(AppPersistence::new(data_dir)), agent_config, log_file)
    }

    fn with_parts(
        persistence: Option<AppPersistence>,
        agent_config_path: Option<PathBuf>,
        log_file: Option<PathBuf>,
    ) -> Self {
        Self {
            workspaces: Vec::new(),
            active_thread_id: None,
            thread_state: HashMap::new(),
            unread_stopped_threads: HashSet::new(),
            log_file,
            persistence,
            agent_config_path,
        }
    }

    pub fn restore_persisted_state(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        self.workspaces = persistence.load()?;
        self.active_thread_id = self
            .workspaces
            .iter()
            .find_map(|workspace| workspace.threads.first())
            .map(|thread| thread.id);
        cx.notify();
        Ok(())
    }

    fn ts(&self, thread_id: Uuid) -> Option<&ThreadState> {
        self.thread_state.get(&thread_id)
    }

    fn ts_mut(&mut self, thread_id: Uuid) -> &mut ThreadState {
        self.thread_state
            .entry(thread_id)
            .or_insert_with(ThreadState::new)
    }

    pub fn add_workspace(&mut self, cx: &mut Context<Self>, name: &str) -> Uuid {
        let workspace = Workspace::new(name);
        let id = workspace.id;
        self.workspaces.push(workspace);
        self.persist_state();
        cx.notify();
        id
    }

    pub fn add_workspace_from_path(&mut self, cx: &mut Context<Self>, path: PathBuf) -> Uuid {
        let workspace = Workspace::from_path(path);
        let id = workspace.id;
        self.workspaces.push(workspace);
        self.persist_state();
        cx.notify();
        id
    }

    pub fn reorder_workspaces(
        &mut self,
        cx: &mut Context<Self>,
        dragged_workspace_id: Uuid,
        target_workspace_id: Uuid,
    ) {
        let Some(from_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == dragged_workspace_id)
        else {
            return;
        };
        let Some(to_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target_workspace_id)
        else {
            return;
        };
        if from_index == to_index {
            return;
        }
        move_vec_item(&mut self.workspaces, from_index, to_index);
        self.persist_state();
        cx.notify();
    }

    pub fn reorder_threads(
        &mut self,
        cx: &mut Context<Self>,
        workspace_id: Uuid,
        dragged_thread_id: Uuid,
        target_thread_id: Uuid,
    ) {
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|item| item.id == workspace_id)
        else {
            return;
        };
        let Some(from_index) = workspace
            .threads
            .iter()
            .position(|thread| thread.id == dragged_thread_id)
        else {
            return;
        };
        let Some(to_index) = workspace
            .threads
            .iter()
            .position(|thread| thread.id == target_thread_id)
        else {
            return;
        };
        if from_index == to_index {
            return;
        }
        move_vec_item(&mut workspace.threads, from_index, to_index);
        self.persist_state();
        cx.notify();
    }

    pub fn add_thread(
        &mut self,
        cx: &mut Context<Self>,
        workspace_id: Uuid,
        name: &str,
    ) -> Option<Uuid> {
        let workspace = self.workspaces.iter_mut().find(|w| w.id == workspace_id)?;
        let thread = Thread::new(workspace_id, name);
        let thread_id = thread.id;
        workspace.add_thread(thread);
        self.active_thread_id = Some(thread_id);

        let (tx, rx) = mpsc::unbounded_channel();
        self.listen_to_agent_events(cx, thread_id, rx);
        self.ts_mut(thread_id).mock_event_sender = Some(tx);

        if let Some(config_path) = self.agent_config_path.clone() {
            self.connect_thread_to_agent_config(cx, thread_id, config_path);
        }

        self.persist_state();
        cx.notify();
        Some(thread_id)
    }

    pub fn set_active_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        self.active_thread_id = Some(thread_id);
        self.unread_stopped_threads.remove(&thread_id);
        if !self
            .thread_state
            .get(&thread_id)
            .is_some_and(|ts| ts.connection.is_some())
            && let Some(config_path) = self.agent_config_path.clone()
        {
            self.connect_thread_to_agent_config(cx, thread_id, config_path);
        }
        cx.notify();
    }

    pub fn rename_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid, name: String) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(thread) = self.find_thread_mut(thread_id) else {
            return false;
        };
        thread.name = trimmed.to_string();
        thread.updated_at = chrono::Utc::now();
        self.persist_state();
        cx.notify();
        true
    }

    pub fn delete_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) -> bool {
        let mut deleted = false;
        for workspace in &mut self.workspaces {
            if let Some(index) = workspace
                .threads
                .iter()
                .position(|thread| thread.id == thread_id)
            {
                workspace.threads.remove(index);
                deleted = true;
                break;
            }
        }
        if !deleted {
            return false;
        }

        self.thread_state.remove(&thread_id);
        self.unread_stopped_threads.remove(&thread_id);

        if self.active_thread_id == Some(thread_id) {
            self.active_thread_id = self
                .workspaces
                .iter()
                .find_map(|workspace| workspace.threads.first())
                .map(|thread| thread.id);
        }

        self.persist_state();
        cx.notify();
        true
    }

    pub fn mark_thread_unread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        self.unread_stopped_threads.insert(thread_id);
        cx.notify();
    }

    pub fn workspace_cwd_for_thread(&self, thread_id: Uuid) -> Option<PathBuf> {
        self.workspaces.iter().find_map(|workspace| {
            workspace
                .threads
                .iter()
                .any(|thread| thread.id == thread_id)
                .then(|| workspace.path.clone())
        })
    }

    pub fn workspace_relative_files_for_thread(
        &self,
        thread_id: Uuid,
        limit: usize,
    ) -> Vec<String> {
        let Some(root) = self.workspace_cwd_for_thread(thread_id) else {
            return Vec::new();
        };
        collect_workspace_files(&root, limit)
    }

    pub fn active_thread_permission_options(&self) -> Option<Vec<PermissionOption>> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id)
            .and_then(|ts| ts.pending_permission.as_ref())
            .map(|r| r.options.clone())
    }

    pub fn active_thread_config_options(&self) -> Option<Vec<SessionConfigOption>> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).map(|ts| ts.config_options.clone())
    }

    pub fn active_thread_available_commands(&self) -> Option<Vec<AvailableCommand>> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).map(|ts| ts.available_commands.clone())
    }

    pub fn active_thread_plan(&self) -> Option<Plan> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).and_then(|ts| ts.plan.clone())
    }

    pub fn active_thread_modes(&self) -> Option<SessionModeState> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).and_then(|ts| ts.mode.clone())
    }

    pub fn thread_has_unread_stop(&self, thread_id: Uuid) -> bool {
        self.unread_stopped_threads.contains(&thread_id)
    }

    pub fn resolve_permission(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        option_id: Option<String>,
    ) {
        self.append_log_line(
            "to_agent.permission_outcome",
            thread_id,
            &serde_json::json!({
                "selected_option_id": option_id.clone(),
            })
            .to_string(),
        );
        if self.resolve_permission_choice(thread_id, option_id) {
            cx.notify();
        }
    }

    pub fn set_session_config_option(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        config_id: String,
        value: String,
    ) {
        let Some(conn) = self.ts(thread_id).and_then(|ts| ts.connection.as_ref()) else {
            return;
        };
        let controller = Rc::clone(&conn.controller);
        let session_id = conn.session_id.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let _ = this.update(&mut cx, |state: &mut AppState, _| {
                        state.append_log_line(
                            "to_agent.set_session_config_option",
                            thread_id,
                            &serde_json::json!({
                                "config_id": config_id.clone(),
                                "value": value.clone(),
                            })
                            .to_string(),
                        );
                    });
                    let result = controller
                        .set_session_config_option(
                            session_id,
                            SessionConfigId::new(config_id),
                            SessionConfigValueId::new(value),
                        )
                        .await;
                    match result {
                        Ok(config_options) => {
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.set_session_config_option",
                                    thread_id,
                                    &serde_json::json!({
                                        "config_options": config_options,
                                    })
                                    .to_string(),
                                );
                                state.ts_mut(thread_id).config_options = config_options;
                                cx.notify();
                            });
                        }
                        Err(err) => {
                            let message = format!("Failed to set session option: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.set_session_config_option_error",
                                    thread_id,
                                    &message,
                                );
                                state.push_system_message(cx, thread_id, message);
                            });
                        }
                    }
                }
            },
        )
        .detach();
    }

    pub fn set_session_mode(&mut self, cx: &mut Context<Self>, thread_id: Uuid, mode_id: String) {
        let Some(conn) = self.ts(thread_id).and_then(|ts| ts.connection.as_ref()) else {
            return;
        };
        let controller = Rc::clone(&conn.controller);
        let session_id = conn.session_id.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let _ = this.update(&mut cx, |state: &mut AppState, _| {
                        state.append_log_line("to_agent.set_session_mode", thread_id, &mode_id);
                    });
                    let result = controller
                        .set_session_mode(session_id, SessionModeId::new(mode_id.clone()))
                        .await;
                    if let Err(err) = result {
                        let message = format!("Failed to set session mode: {err}");
                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                            state.append_log_line(
                                "from_agent.set_session_mode_error",
                                thread_id,
                                &message,
                            );
                            state.push_system_message(cx, thread_id, message);
                        });
                    } else {
                        let _ = this.update(&mut cx, |state: &mut AppState, _| {
                            state.append_log_line(
                                "from_agent.set_session_mode_ok",
                                thread_id,
                                &mode_id,
                            );
                        });
                    }
                }
            },
        )
        .detach();
    }

    pub fn send_user_message(&mut self, cx: &mut Context<Self>, thread_id: Uuid, content: &str) {
        if content.trim().is_empty() {
            return;
        }

        self.append_log_line("to_agent", thread_id, content);

        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::User, content));
        }

        let conn_info = self
            .ts(thread_id)
            .and_then(|ts| ts.connection.as_ref())
            .map(|c| (Rc::clone(&c.controller), c.session_id.clone()));
        let mock_tx = self
            .ts(thread_id)
            .and_then(|ts| ts.mock_event_sender.as_ref())
            .cloned();

        if let Some((controller, session_id)) = conn_info {
            let content = content.to_owned();
            self.ts_mut(thread_id).prompt_started_at = Some(Instant::now());
            cx.spawn(
                move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                    let mut cx = cx.clone();
                    async move {
                        match controller.send_prompt(session_id, content).await {
                            Ok(stop_reason) => {
                                let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                    state.apply_prompt_stop_reason(cx, thread_id, stop_reason);
                                });
                            }
                            Err(err) => {
                                let message = format!("Prompt failed: {err}");
                                let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                    state.ts_mut(thread_id).prompt_started_at = None;
                                    state.push_system_message(cx, thread_id, message);
                                });
                            }
                        }
                    }
                },
            )
            .detach();
        } else if let Some(event_tx) = mock_tx {
            let content = content.to_owned();
            let chunks = ["Mock reply: ", &content];
            for chunk in chunks {
                let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from(chunk.to_owned()),
                ));
                let notification = SessionNotification::new(SessionId::new("mock-session"), update);
                let _ = event_tx.send(AgentEvent::Notification(notification));
            }
            let _ = event_tx.send(AgentEvent::Disconnected);
        }

        cx.notify();
    }

    pub fn connect_thread_to_agent_config(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        config_path: PathBuf,
    ) {
        self.append_log_line(
            "to_agent.connect_from_config",
            thread_id,
            &config_path.display().to_string(),
        );
        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let result = AcpController::connect_from_config(config_path, event_tx).await;

                    match result {
                        Ok((controller, process)) => {
                            let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                state.append_log_line(
                                    "from_agent.connect_ok",
                                    thread_id,
                                    "connected",
                                );
                            });
                            let (cwd, previous_session_id) = this
                                .read_with(&cx, |state, _| {
                                    (
                                        state.workspace_cwd_for_thread(thread_id),
                                        state
                                            .find_thread(thread_id)
                                            .and_then(|thread| thread.session_id.clone()),
                                    )
                                })
                                .ok()
                                .unwrap_or((None, None));

                            let cwd = cwd.unwrap_or_else(|| {
                                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                            });
                            let loaded_session_id = if let Some(session_id) = previous_session_id {
                                let session_id = SessionId::new(session_id);
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.append_log_line(
                                        "to_agent.load_session",
                                        thread_id,
                                        &serde_json::json!({
                                            "session_id": session_id.to_string(),
                                            "cwd": cwd.clone(),
                                        })
                                        .to_string(),
                                    );
                                });
                                match controller
                                    .load_session(session_id.clone(), cwd.clone())
                                    .await
                                {
                                    Ok((config_options, modes)) => {
                                        Some((session_id, config_options, modes))
                                    }
                                    Err(err) => {
                                        let message = format!("Failed to load ACP session: {err}");
                                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                            state.append_log_line(
                                                "from_agent.load_session_error",
                                                thread_id,
                                                &message,
                                            );
                                            state.push_system_message(cx, thread_id, message);
                                        });
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                            let session_result = if let Some(session_data) = loaded_session_id {
                                Ok(session_data)
                            } else {
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.append_log_line(
                                        "to_agent.initialize_session",
                                        thread_id,
                                        &cwd.display().to_string(),
                                    );
                                });
                                controller.initialize_session(cwd).await
                            };

                            match session_result {
                                Ok((session_id, config_options, modes)) => {
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.append_log_line(
                                            "from_agent.initialize_or_load_session_ok",
                                            thread_id,
                                            &serde_json::json!({
                                                "session_id": session_id.to_string(),
                                                "config_options_len": config_options.len(),
                                                "has_modes": modes.is_some(),
                                            })
                                            .to_string(),
                                        );
                                        state.attach_connection(
                                            cx,
                                            thread_id,
                                            controller,
                                            session_id,
                                            config_options,
                                            modes,
                                            Some(process),
                                            event_rx,
                                        );
                                    });
                                }
                                Err(err) => {
                                    let message =
                                        format!("Failed to initialize ACP session: {err}");
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.append_log_line(
                                            "from_agent.initialize_session_error",
                                            thread_id,
                                            &message,
                                        );
                                        state.push_system_message(cx, thread_id, message);
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            let message = format!("Failed to connect ACP controller: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.connect_error",
                                    thread_id,
                                    &message,
                                );
                                state.push_system_message(cx, thread_id, message);
                            });
                        }
                    }
                }
            },
        )
        .detach();
    }

    fn attach_connection(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        controller: AcpController,
        session_id: SessionId,
        config_options: Vec<SessionConfigOption>,
        modes: Option<SessionModeState>,
        process: Option<std::process::Child>,
        rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        self.listen_to_agent_events(cx, thread_id, rx);
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.session_id = Some(session_id.to_string());
        }
        let ts = self.ts_mut(thread_id);
        ts.connection = Some(ThreadAgentConnection {
            controller: Rc::new(controller),
            session_id,
            _process: process,
        });
        ts.config_options = config_options;
        if let Some(modes) = modes {
            ts.mode = Some(modes);
        }
        self.persist_state();
        cx.notify();
    }

    /// Spawns a background task to bridge ACP events into state updates.
    pub fn listen_to_agent_events(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let task = cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    while let Some(event) = rx.recv().await {
                        if this
                            .update(&mut cx, |state: &mut AppState, cx| {
                                state.handle_agent_event(cx, thread_id, event)
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            },
        );

        self.ts_mut(thread_id).agent_task = Some(task);
    }

    pub fn handle_agent_event(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        event: AgentEvent,
    ) {
        self.apply_agent_event(thread_id, event);
        cx.notify();
    }

    pub fn apply_agent_event(&mut self, thread_id: Uuid, event: AgentEvent) {
        match event {
            AgentEvent::Notification(notification) => {
                self.apply_session_update(thread_id, notification.update);
            }
            AgentEvent::PermissionRequest(request) => {
                self.append_log_line(
                    "from_agent.permission_request",
                    thread_id,
                    &serde_json::json!({
                        "options": request.options.clone(),
                    })
                    .to_string(),
                );
                if let Some(existing) = self.ts_mut(thread_id).pending_permission.take() {
                    let _ = existing
                        .response_tx
                        .send(RequestPermissionOutcome::Cancelled);
                }
                self.ts_mut(thread_id).pending_permission = Some(request);
            }
            AgentEvent::Disconnected => {
                self.append_log_line("from_agent.disconnected", thread_id, "disconnected");
                self.finalize_agent_message(thread_id);
            }
        }
    }

    pub fn active_thread(&self) -> Option<&Thread> {
        self.active_thread_id.and_then(|id| self.find_thread(id))
    }

    pub fn active_thread_messages(&self) -> Vec<Message> {
        self.active_thread()
            .map(|thread| thread.messages.clone())
            .unwrap_or_default()
    }

    pub fn active_thread_message_count(&self) -> usize {
        self.active_thread().map(|t| t.messages.len()).unwrap_or(0)
    }

    pub fn active_thread_is_working(&self) -> bool {
        let Some(thread_id) = self.active_thread_id else {
            return false;
        };
        self.ts(thread_id)
            .map(|ts| ts.prompt_started_at.is_some())
            .unwrap_or(false)
    }

    fn find_thread_mut(&mut self, thread_id: Uuid) -> Option<&mut Thread> {
        self.workspaces
            .iter_mut()
            .find_map(|workspace| workspace.get_thread_mut(thread_id))
    }

    fn find_thread(&self, thread_id: Uuid) -> Option<&Thread> {
        self.workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
    }

    fn append_agent_chunk(&mut self, thread_id: Uuid, chunk: &str) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            if let Some(active_message) = thread.get_active_agent_message_mut() {
                active_message.append_text(chunk);
            } else {
                thread.add_message(Message::new(thread_id, Role::Agent, chunk));
            }
        }
    }

    fn finalize_agent_message(&mut self, thread_id: Uuid) {
        if let Some(thread) = self.find_thread_mut(thread_id)
            && let Some(active_message) = thread.get_active_agent_message_mut()
        {
            active_message.finalize();
        }
    }

    fn apply_session_update(&mut self, thread_id: Uuid, update: SessionUpdate) {
        self.log_session_update(thread_id, &update);
        match update {
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(text),
                ..
            }) => {
                self.append_agent_chunk(thread_id, &text.text);
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.ts_mut(thread_id).config_options = update.config_options;
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.ts_mut(thread_id).available_commands = update.available_commands;
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.record_tool_call(thread_id, tool_call);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.apply_tool_call_update(thread_id, update);
            }
            SessionUpdate::Plan(plan) => {
                self.ts_mut(thread_id).plan = Some(plan);
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                let ts = self.ts_mut(thread_id);
                if let Some(mode) = ts.mode.as_mut() {
                    mode.current_mode_id = update.current_mode_id;
                } else {
                    ts.mode = Some(SessionModeState::new(update.current_mode_id, Vec::new()));
                }
            }
            _ => {}
        }
    }

    fn record_tool_call(&mut self, thread_id: Uuid, tool_call: ToolCall) {
        let tool_call_id = tool_call.tool_call_id.to_string();
        let rendered = render_tool_call_message(&tool_call);
        self.ts_mut(thread_id)
            .tool_calls
            .insert(tool_call_id.clone(), tool_call);
        let mut message_id = None;
        if let Some(thread) = self.find_thread_mut(thread_id) {
            let message = Message::new(thread_id, Role::Agent, rendered);
            message_id = Some(message.id);
            thread.add_message(message);
            if let Some(active_message) = thread.get_active_agent_message_mut() {
                active_message.finalize();
            }
        }
        if let Some(message_id) = message_id {
            self.ts_mut(thread_id)
                .tool_call_messages
                .insert(tool_call_id, message_id);
        }
    }

    fn apply_tool_call_update(&mut self, thread_id: Uuid, update: ToolCallUpdate) {
        let tool_call_id = update.tool_call_id.to_string();
        let (rendered, existing_message_id) = {
            let ts = self.ts_mut(thread_id);
            let entry = ts
                .tool_calls
                .entry(tool_call_id.clone())
                .or_insert_with(|| ToolCall::new(update.tool_call_id.clone(), "Tool call"));
            entry.update(update.fields);
            let rendered = render_tool_call_message(entry);
            let existing_message_id = ts.tool_call_messages.get(&tool_call_id).copied();
            (rendered, existing_message_id)
        };
        let mut inserted_message_id = None;
        if let Some(thread) = self.find_thread_mut(thread_id) {
            if let Some(message_id) = existing_message_id
                && let Some(message) = thread.get_message_mut(message_id)
            {
                message.content = rendered;
                message.is_streaming = false;
            } else {
                let message = Message::new(thread_id, Role::Agent, rendered);
                inserted_message_id = Some(message.id);
                thread.add_message(message);
                if let Some(active_message) = thread.get_active_agent_message_mut() {
                    active_message.finalize();
                }
            }
        }
        if let Some(message_id) = inserted_message_id {
            self.ts_mut(thread_id)
                .tool_call_messages
                .insert(tool_call_id, message_id);
        }
    }

    fn apply_prompt_stop_reason(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        stop_reason: StopReason,
    ) {
        self.finalize_agent_message(thread_id);
        let elapsed = self
            .ts_mut(thread_id)
            .prompt_started_at
            .take()
            .map(|started| started.elapsed());
        let elapsed_label = elapsed
            .map(format_duration_short)
            .unwrap_or_else(|| "n/a".to_string());
        let message = format!(
            "Stop reason: {} ({elapsed_label})",
            stop_reason_label(stop_reason)
        );
        if self.active_thread_id != Some(thread_id) {
            self.unread_stopped_threads.insert(thread_id);
        }
        self.push_system_message(cx, thread_id, message);
    }

    fn push_system_message(&mut self, cx: &mut Context<Self>, thread_id: Uuid, content: String) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::System, content));
            cx.notify();
        }
    }

    fn append_log_line(&self, direction: &str, thread_id: Uuid, payload: &str) {
        let Some(log_file) = &self.log_file else {
            return;
        };
        if let Some(parent) = log_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
        else {
            return;
        };
        let record = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "direction": direction,
            "thread_id": thread_id.to_string(),
            "payload": payload,
        });
        let _ = writeln!(file, "{record}");
    }

    fn log_session_update(&self, thread_id: Uuid, update: &SessionUpdate) {
        if let Ok(payload) = serde_json::to_string(update) {
            self.append_log_line("from_agent", thread_id, &payload);
        }
    }

    fn persist_state(&self) {
        if let Some(persistence) = &self.persistence
            && let Err(err) = persistence.save(&self.workspaces)
        {
            eprintln!("failed to persist app state: {err}");
        }
    }

    fn resolve_permission_choice(&mut self, thread_id: Uuid, option_id: Option<String>) -> bool {
        let Some(request) = self.ts_mut(thread_id).pending_permission.take() else {
            return false;
        };
        let outcome = option_id
            .map(|selected| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    PermissionOptionId::new(selected),
                ))
            })
            .unwrap_or(RequestPermissionOutcome::Cancelled);
        let _ = request.response_tx.send(outcome);
        true
    }
}

pub fn extract_text_from_notification(notification: &SessionNotification) -> Option<String> {
    match &notification.update {
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(text),
            ..
        }) => Some(text.text.clone()),
        _ => None,
    }
}

pub fn extract_config_options_from_notification(
    notification: &SessionNotification,
) -> Option<Vec<SessionConfigOption>> {
    match &notification.update {
        SessionUpdate::ConfigOptionUpdate(update) => Some(update.config_options.clone()),
        _ => None,
    }
}

fn render_tool_call_message(tool_call: &ToolCall) -> String {
    let mut lines = vec![
        format!("Tool: {}", tool_call.title),
        format!("Kind: {}", tool_kind_label(tool_call.kind)),
        format!(
            "Status: {}",
            match tool_call.status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::InProgress => "in_progress",
                ToolCallStatus::Completed => "completed",
                ToolCallStatus::Failed => "failed",
                _ => "unknown",
            }
        ),
    ];
    if !tool_call.content.is_empty() {
        for content in &tool_call.content {
            lines.push(render_tool_call_content(content));
        }
    }
    lines.join("\n")
}

fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

fn render_tool_call_content(content: &ToolCallContent) -> String {
    match content {
        ToolCallContent::Content(content) => match &content.content {
            ContentBlock::Text(text) => text.text.clone(),
            _ => "[non-text content]".to_string(),
        },
        ToolCallContent::Diff(diff) => {
            let mut out = vec![format!("Diff: {}", diff.path.display())];
            out.push("```diff".to_string());
            out.push(render_diff_text(diff.old_text.as_deref(), &diff.new_text));
            out.push("```".to_string());
            out.join("\n")
        }
        ToolCallContent::Terminal(terminal) => {
            format!("Terminal: {}", terminal.terminal_id)
        }
        _ => "[unsupported tool content]".to_string(),
    }
}

fn render_diff_text(old: Option<&str>, new: &str) -> String {
    let before = old.unwrap_or_default();
    if before == new {
        return "--- before\n+++ after\n(no changes)".to_string();
    }
    TextDiff::from_lines(before, new)
        .unified_diff()
        .header("before", "after")
        .to_string()
}

fn collect_workspace_files(root: &PathBuf, limit: usize) -> Vec<String> {
    let mut output = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if output.len() >= limit {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        output.push(relative.to_string_lossy().to_string());
    }
    output
}

fn stop_reason_label(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn format_duration_short(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let minutes = (seconds / 60.0).floor() as u64;
    let rem_seconds = (seconds % 60.0).round() as u64;
    format!("{minutes}m {rem_seconds}s")
}

fn move_vec_item<T>(items: &mut Vec<T>, from_index: usize, to_index: usize) {
    let item = items.remove(from_index);
    let adjusted_index = if from_index < to_index {
        to_index.saturating_sub(1)
    } else {
        to_index
    };
    items.insert(adjusted_index, item);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{
        AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, Diff, PermissionOptionKind,
        SessionConfigOption, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields,
    };

    #[test]
    fn incoming_agent_events_update_state() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        let notification = SessionNotification::new(
            SessionId::new("test-session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("hello"))),
        );

        state.apply_agent_event(thread_id, AgentEvent::Notification(notification));
        state.apply_agent_event(thread_id, AgentEvent::Disconnected);

        let thread = state
            .workspaces
            .iter()
            .find_map(|w| w.get_thread(thread_id))
            .expect("thread should exist");

        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].content, "hello");
        assert!(!thread.messages[0].is_streaming);
    }

    #[test]
    fn workspace_cwd_comes_from_thread_workspace() {
        let mut state = AppState::new();
        let mut workspace = Workspace::from_path(PathBuf::from("/tmp/ws-a"));
        let thread = Thread::new(workspace.id, "Thread");
        let thread_id = thread.id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        let cwd = state.workspace_cwd_for_thread(thread_id);
        assert_eq!(cwd, Some(PathBuf::from("/tmp/ws-a")));
    }

    #[test]
    fn stop_reason_label_covers_known_values() {
        assert_eq!(stop_reason_label(StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason_label(StopReason::Cancelled), "cancelled");
    }

    #[test]
    fn format_duration_short_uses_human_readable_units() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_millis(420)),
            "420ms"
        );
        assert_eq!(
            format_duration_short(std::time::Duration::from_millis(1_600)),
            "1.6s"
        );
    }

    #[test]
    fn active_thread_is_working_only_tracks_active_prompts() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let mut thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        thread.add_message(Message::new(thread_id, Role::Agent, "streaming"));
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        assert!(!state.active_thread_is_working());
        state.ts_mut(thread_id).prompt_started_at = Some(Instant::now());
        assert!(state.active_thread_is_working());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn permission_request_round_trip_uses_selected_option() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        let option =
            PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce);
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        state.apply_agent_event(
            thread_id,
            AgentEvent::PermissionRequest(PermissionRequestEvent {
                options: vec![option],
                response_tx,
            }),
        );
        assert_eq!(
            state
                .active_thread_permission_options()
                .expect("request should be active")
                .len(),
            1
        );

        assert!(state.resolve_permission_choice(thread_id, Some("allow_once".to_string())));
        let outcome = response_rx.await.expect("selection should be returned");
        match outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.to_string(), "allow_once");
            }
            RequestPermissionOutcome::Cancelled => panic!("expected selected outcome"),
            _ => panic!("expected selected outcome"),
        }
    }

    #[test]
    fn config_option_update_is_extracted_from_notification() {
        let config_option = SessionConfigOption::select(
            "mode",
            "Mode",
            "default",
            vec![agent_client_protocol::SessionConfigSelectOption::new(
                "default", "Default",
            )],
        );
        let notification = SessionNotification::new(
            SessionId::new("session-1"),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![config_option])),
        );

        let extracted = extract_config_options_from_notification(&notification)
            .expect("config option update should be extracted");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].id.to_string(), "mode");
    }

    #[test]
    fn available_commands_update_is_stored_for_active_thread() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        let notification = SessionNotification::new(
            SessionId::new("session-1"),
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("build", "Run build"),
                AvailableCommand::new("test", "Run tests"),
            ])),
        );
        state.apply_agent_event(thread_id, AgentEvent::Notification(notification));

        let commands = state
            .active_thread_available_commands()
            .expect("commands should be present");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name, "build");
    }

    #[test]
    fn tool_call_updates_render_content_and_diffs() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        let tool_call = ToolCall::new("tool-1", "Read file")
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::from("Reading content")]);
        state.apply_agent_event(
            thread_id,
            AgentEvent::Notification(SessionNotification::new(
                SessionId::new("session-1"),
                SessionUpdate::ToolCall(tool_call),
            )),
        );

        let update = ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::Diff(
                    Diff::new("src/main.rs", "new line").old_text("old line"),
                )]),
        );
        state.apply_agent_event(
            thread_id,
            AgentEvent::Notification(SessionNotification::new(
                SessionId::new("session-1"),
                SessionUpdate::ToolCallUpdate(update),
            )),
        );

        let thread = state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .expect("thread should exist");
        assert_eq!(thread.messages.len(), 1);
        let rendered = thread
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Tool: Read file"));
        assert!(rendered.contains("Kind: other"));
        assert!(rendered.contains("Diff: src/main.rs"));
        assert!(rendered.contains("-old line"));
        assert!(rendered.contains("+new line"));
    }

    #[test]
    fn workspace_relative_files_are_listed_for_thread() {
        let temp_dir =
            std::env::temp_dir().join(format!("acui-files-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(temp_dir.join("src")).expect("should create dir");
        std::fs::write(temp_dir.join("src/main.rs"), "fn main() {}").expect("should write file");

        let mut state = AppState::new();
        let mut workspace = Workspace::from_path(temp_dir.clone());
        let thread = Thread::new(workspace.id, "Thread");
        let thread_id = thread.id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        let files = state.workspace_relative_files_for_thread(thread_id, 20);
        assert!(files.iter().any(|entry| entry.ends_with("src/main.rs")));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_relative_files_respect_gitignore() {
        let temp_dir =
            std::env::temp_dir().join(format!("acui-gitignore-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(temp_dir.join("src")).expect("should create dir");
        std::fs::create_dir_all(temp_dir.join("ignored_dir")).expect("should create ignored dir");
        std::fs::write(temp_dir.join(".gitignore"), "ignored.txt\nignored_dir/\n")
            .expect("should write gitignore");
        std::fs::write(temp_dir.join("src/main.rs"), "fn main() {}").expect("should write file");
        std::fs::write(temp_dir.join("ignored.txt"), "nope").expect("should write ignored file");
        std::fs::write(temp_dir.join("ignored_dir/file.rs"), "nope")
            .expect("should write ignored dir file");

        let mut state = AppState::new();
        let mut workspace = Workspace::from_path(temp_dir.clone());
        let thread = Thread::new(workspace.id, "Thread");
        let thread_id = thread.id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        let files = state.workspace_relative_files_for_thread(thread_id, 100);
        assert!(files.iter().any(|entry| entry.ends_with("src/main.rs")));
        assert!(!files.iter().any(|entry| entry.ends_with("ignored.txt")));
        assert!(!files.iter().any(|entry| entry.contains("ignored_dir")));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn append_log_line_writes_json_records() {
        let temp_dir = std::env::temp_dir().join(format!("acui-log-test-{}", uuid::Uuid::new_v4()));
        let log_path = temp_dir.join("acui.log");
        let state = AppState::with_parts(None, None, Some(log_path.clone()));

        state.append_log_line("to_agent", uuid::Uuid::new_v4(), "hello");
        state.append_log_line("from_agent", uuid::Uuid::new_v4(), "{\"ok\":true}");

        let raw = std::fs::read_to_string(&log_path).expect("log should exist");
        assert!(raw.contains("\"direction\":\"to_agent\""));
        assert!(raw.contains("\"direction\":\"from_agent\""));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn move_vec_item_reorders_items_before_target() {
        let mut values = vec!["a", "b", "c"];
        move_vec_item(&mut values, 2, 0);
        assert_eq!(values, vec!["c", "a", "b"]);
    }
}
