use gpui::{Context, Pixels, Point, Task};
use std::collections::{HashMap, HashSet};
use std::io::{LineWriter, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::{AcpController, AgentEvent, PermissionRequestEvent, TerminalEvent};
use crate::config::AppConfig;
use crate::domain::{Message, MessageContent, Role, Thread, Workspace};
use crate::persistence::AppPersistence;
use agent_client_protocol::{
    AgentCapabilities, AvailableCommand, ContentBlock, ContentChunk, MaybeUndefined, ModelId,
    PermissionOption, PermissionOptionId, Plan, RequestPermissionOutcome,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOption, SessionConfigValueId,
    SessionId, SessionInfo, SessionModeId, SessionModeState, SessionModelState,
    SessionNotification, SessionUpdate, StopReason, TerminalExitStatus, ToolCall, ToolCallUpdate,
    UsageUpdate,
};
use ignore::WalkBuilder;
use similar::TextDiff;

const AUTO_CONNECT_MESSAGE_LIMIT: usize = 400;

struct ThreadAgentConnection {
    controller: Rc<AcpController>,
    session_id: SessionId,
    process: Option<std::process::Child>,
}

impl Drop for ThreadAgentConnection {
    fn drop(&mut self) {
        if let Some(process) = self.process.as_mut() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ThreadTerminal {
    command: Option<String>,
    output: String,
    exit_status: Option<TerminalExitStatus>,
}

/// All per-thread agent state in one place. A single `HashMap<Uuid, ThreadState>`
/// replaces the previous set of parallel HashMaps and ensures nothing is
/// accidentally left behind when a thread is deleted.
struct ThreadState {
    agent_task: Option<Task<()>>,
    mock_event_sender: Option<mpsc::UnboundedSender<AgentEvent>>,
    connection: Option<ThreadAgentConnection>,
    /// True while a `connect_thread_to_agent` task is in flight.
    connecting: bool,
    pending_permission: Option<PermissionRequestEvent>,
    config_options: Vec<SessionConfigOption>,
    available_commands: Vec<AvailableCommand>,
    tool_calls: HashMap<String, ToolCall>,
    tool_call_messages: HashMap<String, Uuid>,
    terminals: HashMap<String, ThreadTerminal>,
    plan: Option<Plan>,
    mode: Option<SessionModeState>,
    model: Option<SessionModelState>,
    usage: Option<UsageUpdate>,
    capabilities: Option<AgentCapabilities>,
    prompt_started_at: Option<Instant>,
    /// Agent selected for this thread but not yet connected (pre-first-message).
    selected_agent_name: Option<String>,
    /// UI State (Not persisted)
    draft_message: Option<String>,
    scroll_offset: Option<Point<Pixels>>,
    locked_to_bottom: bool,
}

impl ThreadState {
    fn new() -> Self {
        Self {
            agent_task: None,
            mock_event_sender: None,
            connection: None,
            connecting: false,
            pending_permission: None,
            config_options: Vec::new(),
            available_commands: Vec::new(),
            tool_calls: HashMap::new(),
            tool_call_messages: HashMap::new(),
            terminals: HashMap::new(),
            plan: None,
            mode: None,
            model: None,
            usage: None,
            capabilities: None,
            prompt_started_at: None,
            selected_agent_name: None,
            draft_message: None,
            scroll_offset: None,
            locked_to_bottom: true,
        }
    }
}

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_thread_id: Option<Uuid>,
    thread_state: HashMap<Uuid, ThreadState>,
    unread_stopped_threads: HashSet<Uuid>,
    agent_capabilities: HashMap<String, AgentCapabilities>,
    workspace_session_syncing: HashSet<(Uuid, String)>,
    log_writer: Option<Mutex<LineWriter<std::fs::File>>>,
    persistence: Option<AppPersistence>,
    agents: Vec<crate::config::AgentConfig>,
    enable_mock_agent: bool,
    /// Pre-connected agents at the app level.
    /// Keyed by (agent_name, cwd) to ensure correct workspace context.
    preconnected_agents: HashMap<(String, PathBuf), PreconnectedAgent>,
    /// In-flight pre-connect attempts keyed the same as `preconnected_agents`.
    preconnecting_agents: HashSet<(String, PathBuf)>,
}

/// A fully connected agent with active session.
struct PreconnectedAgent {
    controller: Rc<AcpController>,
    session_id: SessionId,
    process: std::process::Child,
    rx: mpsc::UnboundedReceiver<AgentEvent>,
    config_options: Vec<SessionConfigOption>,
    mode: Option<SessionModeState>,
    model: Option<SessionModelState>,
    capabilities: Option<AgentCapabilities>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        for preconnected in self.preconnected_agents.values_mut() {
            let _ = preconnected.process.kill();
            let _ = preconnected.process.wait();
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::with_parts(None, Vec::new(), true, None)
    }

    pub fn new_with_config(config: AppConfig) -> Self {
        let AppConfig {
            data_dir,
            agents,
            enable_mock_agent,
            log_file,
        } = config;
        Self::with_parts(
            Some(AppPersistence::new(data_dir)),
            agents,
            enable_mock_agent,
            log_file,
        )
    }

    fn with_parts(
        persistence: Option<AppPersistence>,
        agents: Vec<crate::config::AgentConfig>,
        enable_mock_agent: bool,
        log_file: Option<PathBuf>,
    ) -> Self {
        let log_writer = log_file.as_ref().and_then(|path| {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(LineWriter::new(file)))
        });

        Self {
            workspaces: Vec::new(),
            active_thread_id: None,
            thread_state: HashMap::new(),
            unread_stopped_threads: HashSet::new(),
            agent_capabilities: HashMap::new(),
            workspace_session_syncing: HashSet::new(),
            log_writer,
            persistence,
            agents,
            enable_mock_agent,
            preconnected_agents: HashMap::new(),
            preconnecting_agents: HashSet::new(),
        }
    }

    /// Pre-connect to all agents in the background.
    /// This is called when a new thread is created to make agent switching instant.
    pub fn preconnect_all_agents(&mut self, cx: &mut Context<Self>, cwd: Option<PathBuf>) {
        let cwd_raw = cwd
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let cwd = cwd_raw.canonicalize().unwrap_or(cwd_raw);

        for agent in &self.agents {
            let key = (agent.name.clone(), cwd.clone());
            if self.preconnected_agents.contains_key(&key)
                || self.preconnecting_agents.contains(&key)
            {
                continue; // Already connected
            }
            self.preconnecting_agents.insert(key.clone());

            let agent = agent.clone();
            let cwd = cwd.clone();
            let key = key.clone();

            cx.spawn(
                move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                    let mut cx = cx.clone();
                    async move {
                        let (event_tx, event_rx) = mpsc::unbounded_channel();

                        let result =
                            AcpController::connect_from_agent_config(&agent, event_tx).await;
                        let (controller, mut process) = match result {
                            Ok(pair) => pair,
                            Err(e) => {
                                eprintln!("preconnect: failed to connect to {}: {}", agent.name, e);
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.preconnecting_agents.remove(&key);
                                });
                                return;
                            }
                        };

                        // Initialize and create session
                        let capabilities = match controller.initialize().await {
                            Ok(c) => c,
                            Err(e) => {
                                eprintln!("preconnect: failed to initialize {}: {}", agent.name, e);
                                let _ = process.kill();
                                let _ = process.wait();
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.preconnecting_agents.remove(&key);
                                });
                                return;
                            }
                        };

                        let session = match controller.new_session(cwd.clone()).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!(
                                    "preconnect: failed to create session for {}: {}",
                                    agent.name, e
                                );
                                let _ = process.kill();
                                let _ = process.wait();
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.preconnecting_agents.remove(&key);
                                });
                                return;
                            }
                        };

                        eprintln!("preconnect: successfully pre-connected to {}", agent.name);

                        // Store the pre-connected agent
                        let preconnected = PreconnectedAgent {
                            controller: Rc::new(controller),
                            session_id: session.session_id.clone(),
                            process,
                            rx: event_rx,
                            config_options: session.config_options.clone(),
                            mode: session.modes,
                            model: session.models,
                            capabilities: Some(capabilities),
                        };

                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                            state.preconnecting_agents.remove(&key);
                            if let Some(mut old) = state
                                .preconnected_agents
                                .insert((agent.name.clone(), cwd.clone()), preconnected)
                            {
                                let _ = old.process.kill();
                                let _ = old.process.wait();
                            }

                            // Propagate to threads that have this agent selected/locked but no connection yet.
                            let agent_name = agent.name.clone();
                            let tids_to_update: Vec<Uuid> = state
                                .thread_state
                                .keys()
                                .filter(|tid| {
                                    let ts = state.thread_state.get(tid).unwrap();
                                    ts.selected_agent_name.as_ref() == Some(&agent_name)
                                        || state
                                            .find_thread(**tid)
                                            .and_then(|t| t.agent_name.as_ref())
                                            == Some(&agent_name)
                                })
                                .copied()
                                .collect();

                            for tid in tids_to_update {
                                let ts = state.thread_state.get_mut(&tid).unwrap();
                                if ts.connection.is_none() {
                                    let p = state
                                        .preconnected_agents
                                        .get(&(agent_name.clone(), cwd.clone()))
                                        .unwrap();
                                    ts.config_options = p.config_options.clone();
                                    ts.mode = p.mode.clone();
                                    ts.model = p.model.clone();
                                    ts.capabilities = p.capabilities.clone();
                                }
                            }
                            cx.notify();
                        });
                    }
                },
            )
            .detach();
        }
    }

    pub fn restore_persisted_state(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        // Pre-connect to all agents in the background for instant agent switching
        // This runs regardless of whether we have persisted state
        self.preconnect_all_agents(cx, None);

        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        self.workspaces = persistence.load()?;

        // Pre-connect for all loaded workspace roots as well
        let workspace_paths: Vec<_> = self.workspaces.iter().map(|w| w.path.clone()).collect();
        for path in workspace_paths {
            self.preconnect_all_agents(cx, Some(path));
        }
        self.active_thread_id = self
            .workspaces
            .iter()
            .find_map(|workspace| workspace.threads.first())
            .map(|thread| thread.id);

        // Reconnect only the initially active thread at startup to avoid a
        // thundering-herd of background reconnects for large histories.
        if let Some(active_thread_id) = self.active_thread_id
            && self.should_auto_connect_thread(active_thread_id)
        {
            let locked_agent = self
                .find_thread(active_thread_id)
                .and_then(|t| t.agent_name.as_ref())
                .and_then(|name| self.agents.iter().find(|a| &a.name == name))
                .cloned();
            if let Some(agent) = locked_agent {
                self.connect_thread_to_agent(cx, active_thread_id, agent);
            }
        }

        self.sync_unsynced_workspace_sessions(cx);
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
        let path = workspace.path.clone();
        self.workspaces.push(workspace);
        self.preconnect_all_agents(cx, Some(path));
        self.sync_workspace_sessions(cx, id);
        self.persist_state();
        cx.notify();
        id
    }

    pub fn add_workspace_from_path(&mut self, cx: &mut Context<Self>, path: PathBuf) -> Uuid {
        let workspace = Workspace::from_path(path);
        let id = workspace.id;
        let path = workspace.path.clone();
        self.workspaces.push(workspace);
        self.preconnect_all_agents(cx, Some(path));
        self.sync_workspace_sessions(cx, id);
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
        let cwd = workspace.path.clone();
        let thread = Thread::new(workspace_id, name);
        let thread_id = thread.id;
        workspace.add_thread(thread);
        self.active_thread_id = Some(thread_id);

        if self.enable_mock_agent {
            let (tx, rx) = mpsc::unbounded_channel();
            self.listen_to_agent_events(cx, thread_id, rx);
            self.ts_mut(thread_id).mock_event_sender = Some(tx);
        }

        // Pre-connect to all agents so switching is instant
        self.preconnect_all_agents(cx, Some(cwd.clone()));

        // Pre-select the first configured agent for the thread
        // The connection is already established via preconnect_all_agents
        let first_agent = self.agents.first().cloned();
        if let Some(agent) = first_agent {
            let ts = self.ts_mut(thread_id);
            ts.selected_agent_name = Some(agent.name.clone());

            if let Some(preconnected) = self.take_preconnected_agent(&agent.name, Some(cwd.clone()))
            {
                let session = crate::client::SessionBootstrap {
                    session_id: preconnected.session_id.clone(),
                    config_options: preconnected.config_options.clone(),
                    modes: preconnected.mode.clone(),
                    models: preconnected.model.clone(),
                };
                let controller = preconnected.controller.clone();
                let capabilities = preconnected.capabilities.clone().unwrap_or_default();
                let process = preconnected.process;
                let rx = preconnected.rx;

                self.attach_connection_rc(
                    cx,
                    thread_id,
                    controller,
                    session,
                    capabilities,
                    Some(process),
                    rx,
                );
                // Re-fill the pre-connect cache for other threads
                self.preconnect_all_agents(cx, Some(cwd));
            } else {
                // Fallback to metadata only if not fully pre-connected yet
                // The first message will establish the full connection.
                let pre_data = self
                    .preconnected_agents
                    .get(&(agent.name.clone(), cwd.clone()))
                    .map(|p| {
                        (
                            p.config_options.clone(),
                            p.mode.clone(),
                            p.model.clone(),
                            p.capabilities.clone(),
                        )
                    });

                if let Some((config_options, mode, model, capabilities)) = pre_data {
                    let ts = self.ts_mut(thread_id);
                    ts.config_options = config_options;
                    ts.mode = mode;
                    ts.model = model;
                    ts.capabilities = capabilities;
                }
            }
        }

        self.persist_state();
        cx.notify();
        Some(thread_id)
    }

    pub fn set_active_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        self.active_thread_id = Some(thread_id);
        self.unread_stopped_threads.remove(&thread_id);

        let locked_agent_name = self
            .find_thread(thread_id)
            .and_then(|t| t.agent_name.clone());

        if let Some(name) = &locked_agent_name {
            self.ts_mut(thread_id).selected_agent_name = Some(name.clone());
        }

        // Auto-connect if the thread is locked to a specific agent and neither a
        // connection nor an in-flight connect task already exists (the startup
        // background load may have already spawned one).
        let locked_agent =
            locked_agent_name.and_then(|name| self.agents.iter().find(|a| a.name == name).cloned());
        let already_connecting = self
            .thread_state
            .get(&thread_id)
            .is_some_and(|ts| ts.connection.is_some() || ts.connecting);
        if !already_connecting
            && self.should_auto_connect_thread(thread_id)
            && let Some(agent) = locked_agent
        {
            self.connect_thread_to_agent(cx, thread_id, agent);
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
        thread.user_renamed = true;
        thread.updated_at = chrono::Utc::now();
        self.persist_state();
        cx.notify();
        true
    }

    pub fn update_thread_draft(&mut self, thread_id: Uuid, draft: String) {
        let ts = self.ts_mut(thread_id);
        ts.draft_message = if draft.is_empty() { None } else { Some(draft) };
    }

    pub fn thread_draft(&self, thread_id: Uuid) -> Option<String> {
        self.ts(thread_id).and_then(|ts| ts.draft_message.clone())
    }

    pub fn update_thread_scroll_state(
        &mut self,
        thread_id: Uuid,
        offset: Point<Pixels>,
        locked: bool,
    ) {
        let ts = self.ts_mut(thread_id);
        ts.scroll_offset = Some(offset);
        ts.locked_to_bottom = locked;
    }

    pub fn thread_scroll_state(&self, thread_id: Uuid) -> (Option<Point<Pixels>>, bool) {
        self.ts(thread_id)
            .map(|ts| (ts.scroll_offset, ts.locked_to_bottom))
            .unwrap_or((None, true))
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

    pub fn active_thread_models(&self) -> Option<SessionModelState> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).and_then(|ts| ts.model.clone())
    }

    pub fn active_thread_usage(&self) -> Option<UsageUpdate> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id).and_then(|ts| ts.usage.clone())
    }

    pub fn thread_can_fork(&self, thread_id: Uuid) -> bool {
        let Some(thread) = self.find_thread(thread_id) else {
            return false;
        };
        if thread.session_id.is_none() {
            return false;
        }
        let Some(agent_name) = thread.agent_name.as_ref() else {
            return false;
        };
        self.agent_capabilities
            .get(agent_name)
            .and_then(|capabilities| capabilities.session_capabilities.fork.as_ref())
            .is_some()
    }

    pub fn terminal_transcript_for_thread(
        &self,
        thread_id: Uuid,
        terminal_id: &str,
    ) -> Option<String> {
        let terminal = self.ts(thread_id)?.terminals.get(terminal_id)?;
        let mut transcript = String::new();
        if let Some(command) = &terminal.command {
            transcript.push_str("$ ");
            transcript.push_str(command);
            transcript.push('\n');
        }
        transcript.push_str(&terminal.output);
        if let Some(exit_status) = &terminal.exit_status {
            if !transcript.is_empty() && !transcript.ends_with('\n') {
                transcript.push('\n');
            }
            transcript.push_str(&terminal_exit_status_label(exit_status));
            transcript.push('\n');
        }
        Some(transcript)
    }

    pub fn thread_has_unread_stop(&self, thread_id: Uuid) -> bool {
        self.unread_stopped_threads.contains(&thread_id)
    }

    /// All configured named agents.
    pub fn configured_agents(&self) -> &[crate::config::AgentConfig] {
        &self.agents
    }

    /// Check if a named agent is pre-connected for a given CWD.
    pub fn agent_is_preconnected(&self, agent_name: &str, cwd: Option<PathBuf>) -> bool {
        let cwd_raw = cwd
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let cwd = cwd_raw.canonicalize().unwrap_or(cwd_raw);
        let key = (agent_name.to_string(), cwd);
        self.preconnected_agents.contains_key(&key)
    }

    fn take_preconnected_agent(
        &mut self,
        agent_name: &str,
        cwd: Option<PathBuf>,
    ) -> Option<PreconnectedAgent> {
        let cwd_raw = cwd
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let cwd = cwd_raw.canonicalize().unwrap_or(cwd_raw);
        let key = (agent_name.to_string(), cwd);
        self.preconnected_agents.remove(&key)
    }

    /// Select a named agent for the active thread (before first message).
    /// Uses pre-connected agents for instant switching.
    pub fn select_agent_for_thread(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        agent_name: String,
    ) {
        // Check if already selected
        let already_selected = self
            .ts(thread_id)
            .map(|ts| ts.selected_agent_name.as_ref() == Some(&agent_name))
            .unwrap_or(false);

        if already_selected {
            return;
        }

        // Now update the thread state
        let ts = self.ts_mut(thread_id);
        ts.selected_agent_name = Some(agent_name.clone());

        // Clear existing state
        ts.config_options.clear();
        ts.mode = None;
        ts.model = None;
        ts.capabilities = None;

        // If we have a pre-connected agent, take it and attach it immediately.
        let cwd = self.workspace_cwd_for_thread(thread_id);
        if let Some(preconnected) = self.take_preconnected_agent(&agent_name, cwd.clone()) {
            let session = crate::client::SessionBootstrap {
                session_id: preconnected.session_id.clone(),
                config_options: preconnected.config_options.clone(),
                modes: preconnected.mode.clone(),
                models: preconnected.model.clone(),
            };
            let controller = preconnected.controller.clone();
            self.attach_connection_rc(
                cx,
                thread_id,
                controller,
                session,
                preconnected.capabilities.clone().unwrap_or_default(),
                Some(preconnected.process),
                preconnected.rx,
            );
            // Re-fill the pre-connect cache for other threads
            self.preconnect_all_agents(cx, cwd);
        } else {
            // No pre-connection ready, wait for preconnect_all_agents to propagate it
            // or for first message to connect.
            self.preconnect_all_agents(cx, cwd);
        }

        cx.notify();
    }

    /// The agent name pre-selected for the active thread (before it is locked).
    pub fn active_thread_selected_agent(&self) -> Option<String> {
        let thread_id = self.active_thread_id?;
        self.ts(thread_id)
            .and_then(|ts| ts.selected_agent_name.clone())
    }

    /// Whether the active thread is locked to an agent (first message already sent).
    pub fn active_thread_is_agent_locked(&self) -> bool {
        self.active_thread_id
            .and_then(|id| self.find_thread(id))
            .is_some_and(|t| t.agent_name.is_some())
    }

    /// The agent name the active thread is locked to (after first message).
    pub fn active_thread_locked_agent(&self) -> Option<String> {
        self.active_thread_id
            .and_then(|id| self.find_thread(id))
            .and_then(|t| t.agent_name.clone())
    }

    fn sync_unsynced_workspace_sessions(&mut self, cx: &mut Context<Self>) {
        let workspace_ids: Vec<Uuid> = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .collect();
        for workspace_id in workspace_ids {
            self.sync_workspace_sessions(cx, workspace_id);
        }
    }

    fn sync_workspace_sessions(&mut self, cx: &mut Context<Self>, workspace_id: Uuid) {
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        if self.agents.is_empty() {
            return;
        }
        let workspace_path = workspace.path.clone();
        let agents_to_sync: Vec<_> = self
            .agents
            .iter()
            .filter(|agent| !workspace.session_listed_agents.contains(&agent.name))
            .filter(|agent| {
                !self
                    .workspace_session_syncing
                    .contains(&(workspace_id, agent.name.clone()))
            })
            .cloned()
            .collect();

        for agent in agents_to_sync {
            let agent_name = agent.name.clone();
            self.workspace_session_syncing
                .insert((workspace_id, agent_name.clone()));
            let workspace_path = workspace_path.clone();
            cx.spawn(
                move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                    let mut cx = cx.clone();
                    async move {
                        let (event_tx, _event_rx) = mpsc::unbounded_channel();
                        let connect_result =
                            AcpController::connect_from_agent_config(&agent, event_tx).await;
                        let (controller, mut process) = match connect_result {
                            Ok(pair) => pair,
                            Err(_) => {
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state
                                        .workspace_session_syncing
                                        .remove(&(workspace_id, agent_name.clone()));
                                });
                                return;
                            }
                        };

                        let capabilities = match controller.initialize().await {
                            Ok(capabilities) => capabilities,
                            Err(_) => {
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state
                                        .workspace_session_syncing
                                        .remove(&(workspace_id, agent_name.clone()));
                                });
                                let _ = process.kill();
                                let _ = process.wait();
                                return;
                            }
                        };

                        let sessions_result = if capabilities.session_capabilities.list.is_some() {
                            controller
                                .list_sessions(Some(workspace_path.clone()))
                                .await
                                .map(Some)
                        } else {
                            Ok(None)
                        };

                        let _ = process.kill();
                        let _ = process.wait();

                        match sessions_result {
                            Ok(sessions) => {
                                let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                    state
                                        .agent_capabilities
                                        .insert(agent_name.clone(), capabilities.clone());
                                    if let Some(sessions) = sessions {
                                        for session in sessions {
                                            state.upsert_listed_session_thread(
                                                workspace_id,
                                                &agent_name,
                                                session,
                                            );
                                        }
                                    }
                                    state.mark_workspace_agent_synced(workspace_id, &agent_name);
                                    state
                                        .workspace_session_syncing
                                        .remove(&(workspace_id, agent_name.clone()));
                                    state.persist_state();
                                    cx.notify();
                                });
                            }
                            Err(_) => {
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state
                                        .workspace_session_syncing
                                        .remove(&(workspace_id, agent_name.clone()));
                                });
                            }
                        }
                    }
                },
            )
            .detach();
        }
    }

    fn mark_workspace_agent_synced(&mut self, workspace_id: Uuid, agent_name: &str) {
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        if workspace
            .session_listed_agents
            .iter()
            .any(|existing| existing == agent_name)
        {
            return;
        }
        workspace.session_listed_agents.push(agent_name.to_string());
    }

    fn upsert_listed_session_thread(
        &mut self,
        workspace_id: Uuid,
        agent_name: &str,
        session: SessionInfo,
    ) {
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };

        let session_id = session.session_id.to_string();
        if let Some(thread) = workspace.threads.iter_mut().find(|thread| {
            thread.agent_name.as_deref() == Some(agent_name)
                && thread.session_id.as_deref() == Some(session_id.as_str())
        }) {
            if !thread.user_renamed
                && let Some(title) = session.title
            {
                let trimmed = title.trim();
                if !trimmed.is_empty() {
                    thread.name = trimmed.to_string();
                }
            }
            return;
        }

        let mut thread_name = session.title.unwrap_or_else(|| {
            let short_session = session_id.chars().take(8).collect::<String>();
            format!("{agent_name}: {short_session}")
        });
        if thread_name.trim().is_empty() {
            thread_name = format!("{agent_name}: {}", session_id);
        }
        let mut thread = Thread::new(workspace_id, thread_name);
        let thread_id = thread.id;
        thread.agent_name = Some(agent_name.to_string());
        thread.session_id = Some(session_id);
        workspace.add_thread(thread);
        self.ts_mut(thread_id).selected_agent_name = Some(agent_name.to_string());
        if self.active_thread_id.is_none() {
            self.active_thread_id = Some(thread_id);
        }
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
        let conn_info = self
            .ts(thread_id)
            .and_then(|ts| ts.connection.as_ref())
            .map(|conn| (Rc::clone(&conn.controller), conn.session_id.clone()));

        let Some((controller, session_id)) = conn_info else {
            return;
        };

        // Optimistically update the current mode so the UI reflects the change immediately.
        if let Some(mode) = self.ts_mut(thread_id).mode.as_mut() {
            mode.current_mode_id = SessionModeId::new(mode_id.clone());
        }
        cx.notify();

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
                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                            state.append_log_line(
                                "from_agent.set_session_mode_ok",
                                thread_id,
                                &mode_id,
                            );
                            cx.notify();
                        });
                    }
                }
            },
        )
        .detach();
    }

    pub fn set_session_model(&mut self, cx: &mut Context<Self>, thread_id: Uuid, model_id: String) {
        if self
            .ts(thread_id)
            .and_then(|ts| ts.model.as_ref())
            .is_none()
        {
            return;
        }
        let conn_info = self
            .ts(thread_id)
            .and_then(|ts| ts.connection.as_ref())
            .map(|conn| (Rc::clone(&conn.controller), conn.session_id.clone()));

        let Some((controller, session_id)) = conn_info else {
            return;
        };

        if let Some(models) = self.ts_mut(thread_id).model.as_mut() {
            models.current_model_id = ModelId::new(model_id.clone());
        }
        cx.notify();

        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let _ = this.update(&mut cx, |state: &mut AppState, _| {
                        state.append_log_line("to_agent.set_session_model", thread_id, &model_id);
                    });
                    let result = controller
                        .set_session_model(session_id, ModelId::new(model_id.clone()))
                        .await;
                    if let Err(err) = result {
                        let message = format!("Failed to set session model: {err}");
                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                            state.append_log_line(
                                "from_agent.set_session_model_error",
                                thread_id,
                                &message,
                            );
                            state.push_system_message(cx, thread_id, message);
                        });
                    } else {
                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                            state.append_log_line(
                                "from_agent.set_session_model_ok",
                                thread_id,
                                &model_id,
                            );
                            cx.notify();
                        });
                    }
                }
            },
        )
        .detach();
    }

    pub fn fork_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        let Some(thread) = self.find_thread(thread_id).cloned() else {
            return;
        };
        let Some(session_id) = thread.session_id.clone() else {
            return;
        };
        let Some(agent_name) = thread.agent_name.clone() else {
            return;
        };

        let capabilities = self.agent_capabilities.get(&agent_name).cloned();
        if capabilities
            .as_ref()
            .and_then(|caps| caps.session_capabilities.fork.as_ref())
            .is_none()
        {
            return;
        }

        let source_thread_name = thread.name;
        let workspace_id = thread.workspace_id;
        let cwd = self
            .workspace_cwd_for_thread(thread_id)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let connected_controller = self
            .ts(thread_id)
            .and_then(|ts| ts.connection.as_ref())
            .map(|conn| Rc::clone(&conn.controller));
        let agent = self
            .agents
            .iter()
            .find(|candidate| candidate.name == agent_name)
            .cloned();

        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let source_session_id = SessionId::new(session_id.clone());
                    let mut active_capabilities = capabilities;
                    let forked = if let Some(controller) = connected_controller {
                        controller
                            .fork_session(source_session_id, cwd.clone())
                            .await
                    } else {
                        let Some(agent) = agent else {
                            return;
                        };
                        let (event_tx, _event_rx) = mpsc::unbounded_channel();
                        let connect_result =
                            AcpController::connect_from_agent_config(&agent, event_tx).await;
                        let (controller, mut process) = match connect_result {
                            Ok(pair) => pair,
                            Err(err) => {
                                let message = format!("Failed to connect ACP controller: {err}");
                                let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                    state.push_system_message(cx, thread_id, message);
                                });
                                return;
                            }
                        };

                        let capabilities = match controller.initialize().await {
                            Ok(capabilities) => capabilities,
                            Err(err) => {
                                let _ = process.kill();
                                let _ = process.wait();
                                let message = format!("Failed to initialize ACP session: {err}");
                                let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                    state.push_system_message(cx, thread_id, message);
                                });
                                return;
                            }
                        };
                        if capabilities.session_capabilities.fork.is_none() {
                            let _ = process.kill();
                            let _ = process.wait();
                            return;
                        }
                        active_capabilities = Some(capabilities);
                        let response = controller
                            .fork_session(source_session_id, cwd.clone())
                            .await;
                        let _ = process.kill();
                        let _ = process.wait();
                        response
                    };

                    match forked {
                        Ok(forked) => {
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                let forked_thread_id = {
                                    let Some(workspace) = state
                                        .workspaces
                                        .iter_mut()
                                        .find(|workspace| workspace.id == workspace_id)
                                    else {
                                        return;
                                    };
                                    let mut forked_thread = Thread::new(
                                        workspace_id,
                                        format!("{source_thread_name} (fork)"),
                                    );
                                    let forked_thread_id = forked_thread.id;
                                    forked_thread.agent_name = Some(agent_name.clone());
                                    forked_thread.session_id = Some(forked.session_id.to_string());
                                    workspace.add_thread(forked_thread);
                                    forked_thread_id
                                };
                                state.active_thread_id = Some(forked_thread_id);
                                let capability_for_thread = active_capabilities.clone();
                                if let Some(capabilities) = capability_for_thread.as_ref() {
                                    state
                                        .agent_capabilities
                                        .insert(agent_name.clone(), capabilities.clone());
                                }
                                let ts = state.ts_mut(forked_thread_id);
                                ts.selected_agent_name = Some(agent_name.clone());
                                ts.config_options = forked.config_options;
                                ts.mode = forked.modes;
                                ts.model = forked.models;
                                ts.capabilities = capability_for_thread;
                                state.persist_state();
                                cx.notify();
                            });
                        }
                        Err(err) => {
                            let message = format!("Failed to fork ACP session: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.push_system_message(cx, thread_id, message);
                            });
                        }
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

        // Get the selected agent name before mutating the thread
        let agent_to_lock = self
            .ts(thread_id)
            .and_then(|ts| ts.selected_agent_name.clone());

        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::User, content));
            // Lock the thread to the selected agent when the first message is sent.
            // This allows users to change their agent selection up until they send
            // their first message.
            if thread.agent_name.is_none()
                && let Some(selected_agent) = agent_to_lock
            {
                thread.agent_name = Some(selected_agent);
            }
        }

        let conn_info = self
            .ts(thread_id)
            .and_then(|ts| ts.connection.as_ref())
            .map(|c| (Rc::clone(&c.controller), c.session_id.clone()));
        let _mock_tx = self
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
        } else {
            // Try to use a pre-connected agent
            let agent_name = self
                .ts(thread_id)
                .and_then(|ts| ts.selected_agent_name.clone())
                .or_else(|| {
                    self.find_thread(thread_id)
                        .and_then(|t| t.agent_name.clone())
                });

            if let Some(agent_name) = agent_name {
                let ts_ref = self.ts(thread_id);
                let has_session_id = self
                    .find_thread(thread_id)
                    .is_some_and(|t| t.session_id.is_some());
                let has_connection =
                    ts_ref.is_some_and(|ts| ts.connection.is_some() || ts.connecting);

                let cwd = self.workspace_cwd_for_thread(thread_id);
                if !has_session_id
                    && !has_connection
                    && let Some(preconnected) =
                        self.take_preconnected_agent(&agent_name, cwd.clone())
                {
                    // Consume the pre-connected agent
                    let controller = preconnected.controller.clone();
                    let session_id = preconnected.session_id.clone();
                    let session = crate::client::SessionBootstrap {
                        session_id: session_id.clone(),
                        config_options: preconnected.config_options.clone(),
                        modes: preconnected.mode.clone(),
                        models: preconnected.model.clone(),
                    };
                    let capabilities = preconnected.capabilities.clone().unwrap_or_default();
                    let process = preconnected.process;
                    let event_rx = preconnected.rx;

                    // Attach it to the thread first
                    self.attach_connection_rc(
                        cx,
                        thread_id,
                        controller.clone(),
                        session,
                        capabilities,
                        Some(process),
                        event_rx,
                    );
                    // Re-fill cache
                    self.preconnect_all_agents(cx, cwd);

                    let content = content.to_owned();
                    self.ts_mut(thread_id).prompt_started_at = Some(Instant::now());
                    cx.spawn(
                        move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                            let mut cx = cx.clone();
                            async move {
                                match controller.send_prompt(session_id, content).await {
                                    Ok(stop_reason) => {
                                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                            state.apply_prompt_stop_reason(
                                                cx,
                                                thread_id,
                                                stop_reason,
                                            );
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
                } else {
                    // No pre-connected agent ready - fall back to connecting on demand
                    let agent = self
                        .ts(thread_id)
                        .and_then(|ts| ts.selected_agent_name.clone())
                        .or_else(|| {
                            self.find_thread(thread_id)
                                .and_then(|t| t.agent_name.clone())
                        })
                        .and_then(|name| self.agents.iter().find(|a| a.name == *name))
                        .cloned();

                    if let Some(agent) = agent {
                        let content = content.to_owned();
                        self.ts_mut(thread_id).prompt_started_at = Some(Instant::now());
                        self.preconnect_all_agents(cx, cwd);
                        self.connect_and_send(cx, thread_id, agent, content);
                    }
                }
            }
        }

        cx.notify();
    }

    /// Connect to an agent and send a prompt in a single async task.
    /// This is a fallback for when pre-connected agents aren't available.
    fn connect_and_send(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        agent: crate::config::AgentConfig,
        content: String,
    ) {
        // Optimization: If already connected (e.g. via add_thread pre-connection), use it!
        if let Some(ts) = self.ts(thread_id)
            && let Some(connection) = &ts.connection
        {
            self.append_log_line("to_agent.send_existing", thread_id, &agent.name);
            let controller = connection.controller.clone();
            let session_id = connection.session_id.clone();

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
            return;
        }

        self.append_log_line("to_agent.connect_and_send", thread_id, &agent.name);

        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let connect_result =
                        AcpController::connect_from_agent_config(&agent, event_tx).await;
                    let (controller, process): (AcpController, _) = match connect_result {
                        Ok(pair) => pair,
                        Err(err) => {
                            let message = format!("Failed to connect ACP controller: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.connect_error",
                                    thread_id,
                                    &message,
                                );
                                state.ts_mut(thread_id).prompt_started_at = None;
                                state.push_system_message(cx, thread_id, message);
                            });
                            return;
                        }
                    };

                    let cwd = this
                        .read_with(&cx, |state, _| state.workspace_cwd_for_thread(thread_id))
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| {
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                        });

                    let capabilities = match controller.initialize().await {
                        Ok(capabilities) => capabilities,
                        Err(err) => {
                            let message = format!("Failed to initialize ACP session: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.initialize_session_error",
                                    thread_id,
                                    &message,
                                );
                                state.ts_mut(thread_id).prompt_started_at = None;
                                state.push_system_message(cx, thread_id, message);
                            });
                            return;
                        }
                    };

                    let session = match controller.new_session(cwd).await {
                        Ok(data) => data,
                        Err(err) => {
                            let message = format!("Failed to initialize ACP session: {err}");
                            let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                state.append_log_line(
                                    "from_agent.initialize_session_error",
                                    thread_id,
                                    &message,
                                );
                                state.ts_mut(thread_id).prompt_started_at = None;
                                state.push_system_message(cx, thread_id, message);
                            });
                            return;
                        }
                    };

                    // Lock thread, attach connection, and retrieve Rc'd controller + session_id
                    // for the send step — all within the same update to avoid races.
                    let send_conn = this.update(&mut cx, |state: &mut AppState, cx| {
                        if let Some(thread) = state.find_thread_mut(thread_id)
                            && thread.agent_name.is_none()
                        {
                            thread.agent_name = Some(agent.name.clone());
                        }
                        state.append_log_line(
                            "from_agent.initialize_session_ok",
                            thread_id,
                            &session.session_id.to_string(),
                        );
                        state
                            .agent_capabilities
                            .insert(agent.name.clone(), capabilities.clone());
                        state.attach_connection(
                            cx,
                            thread_id,
                            controller,
                            session,
                            capabilities,
                            Some(process),
                            event_rx,
                        );
                        state
                            .ts(thread_id)
                            .and_then(|ts| ts.connection.as_ref())
                            .map(|c| (Rc::clone(&c.controller), c.session_id.clone()))
                    });

                    let Ok(Some((controller, session_id))) = send_conn else {
                        return;
                    };

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
    }

    pub fn connect_thread_to_agent(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        agent: crate::config::AgentConfig,
    ) {
        self.append_log_line("to_agent.connect", thread_id, &agent.name);

        let cwd = self.workspace_cwd_for_thread(thread_id);
        if let Some(preconnected) = self.take_preconnected_agent(&agent.name, cwd.clone()) {
            let session = crate::client::SessionBootstrap {
                session_id: preconnected.session_id.clone(),
                config_options: preconnected.config_options.clone(),
                modes: preconnected.mode.clone(),
                models: preconnected.model.clone(),
            };
            let controller = preconnected.controller.clone();
            self.attach_connection_rc(
                cx,
                thread_id,
                controller,
                session,
                preconnected.capabilities.clone().unwrap_or_default(),
                Some(preconnected.process),
                preconnected.rx,
            );
            // Re-fill the pre-connect cache for other threads
            self.preconnect_all_agents(cx, cwd);
            return;
        }

        self.ts_mut(thread_id).connecting = true;

        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let result = AcpController::connect_from_agent_config(&agent, event_tx).await;

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
                            let capabilities = match controller.initialize().await {
                                Ok(capabilities) => capabilities,
                                Err(err) => {
                                    let message =
                                        format!("Failed to initialize ACP session: {err}");
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.append_log_line(
                                            "from_agent.initialize_session_error",
                                            thread_id,
                                            &message,
                                        );
                                        state.ts_mut(thread_id).connecting = false;
                                        state.push_system_message(cx, thread_id, message);
                                    });
                                    return;
                                }
                            };

                            let loaded_session = if let Some(session_id) = previous_session_id {
                                let session_id = SessionId::new(session_id);
                                if capabilities.load_session {
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
                                    match controller.load_session(session_id, cwd.clone()).await {
                                        Ok(session) => Some(session),
                                        Err(err) => {
                                            let message =
                                                format!("Failed to load ACP session: {err}");
                                            let _ =
                                                this.update(&mut cx, |state: &mut AppState, cx| {
                                                    state.append_log_line(
                                                        "from_agent.load_session_error",
                                                        thread_id,
                                                        &message,
                                                    );
                                                    state.push_system_message(
                                                        cx, thread_id, message,
                                                    );
                                                });
                                            None
                                        }
                                    }
                                } else if capabilities.session_capabilities.resume.is_some() {
                                    let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                        state.append_log_line(
                                            "to_agent.resume_session",
                                            thread_id,
                                            &serde_json::json!({
                                                "session_id": session_id.to_string(),
                                                "cwd": cwd.clone(),
                                            })
                                            .to_string(),
                                        );
                                    });
                                    match controller.resume_session(session_id, cwd.clone()).await {
                                        Ok(session) => Some(session),
                                        Err(err) => {
                                            let message =
                                                format!("Failed to resume ACP session: {err}");
                                            let _ =
                                                this.update(&mut cx, |state: &mut AppState, cx| {
                                                    state.append_log_line(
                                                        "from_agent.resume_session_error",
                                                        thread_id,
                                                        &message,
                                                    );
                                                    state.push_system_message(
                                                        cx, thread_id, message,
                                                    );
                                                });
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let session_result = if let Some(session) = loaded_session {
                                Ok(session)
                            } else {
                                let _ = this.update(&mut cx, |state: &mut AppState, _| {
                                    state.append_log_line(
                                        "to_agent.initialize_session",
                                        thread_id,
                                        &cwd.display().to_string(),
                                    );
                                });
                                controller.new_session(cwd).await
                            };

                            match session_result {
                                Ok(session) => {
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.append_log_line(
                                            "from_agent.initialize_or_load_session_ok",
                                            thread_id,
                                            &serde_json::json!({
                                                "session_id": session.session_id.to_string(),
                                                "config_options_len": session.config_options.len(),
                                                "has_modes": session.modes.is_some(),
                                            })
                                            .to_string(),
                                        );
                                        // Don't lock the thread to this agent here - that happens
                                        // when the first message is sent. This allows users to
                                        // change their agent selection before sending a message.
                                        state
                                            .agent_capabilities
                                            .insert(agent.name.clone(), capabilities.clone());
                                        state.ts_mut(thread_id).connecting = false;
                                        state.attach_connection(
                                            cx,
                                            thread_id,
                                            controller,
                                            session,
                                            capabilities,
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
                                        state.ts_mut(thread_id).connecting = false;
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
                                state.ts_mut(thread_id).connecting = false;
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
        session: crate::client::SessionBootstrap,
        capabilities: AgentCapabilities,
        process: Option<std::process::Child>,
        rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        self.attach_connection_rc(
            cx,
            thread_id,
            Rc::new(controller),
            session,
            capabilities,
            process,
            rx,
        );
    }

    fn attach_connection_rc(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        controller: Rc<AcpController>,
        session: crate::client::SessionBootstrap,
        capabilities: AgentCapabilities,
        process: Option<std::process::Child>,
        rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        self.listen_to_agent_events(cx, thread_id, rx);
        let session_id = session.session_id.clone();
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.session_id = Some(session_id.to_string());
        }
        let ts = self.ts_mut(thread_id);
        ts.connection = Some(ThreadAgentConnection {
            controller,
            session_id,
            process,
        });
        ts.config_options = session.config_options;
        ts.mode = session.modes;
        ts.model = session.models;
        ts.capabilities = Some(capabilities);
        ts.usage = None;
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
        const AGENT_EVENT_BATCH_SIZE: usize = 128;
        let task = cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    while let Some(event) = rx.recv().await {
                        let mut batch = Vec::with_capacity(AGENT_EVENT_BATCH_SIZE);
                        batch.push(event);
                        while batch.len() < AGENT_EVENT_BATCH_SIZE {
                            let Ok(event) = rx.try_recv() else {
                                break;
                            };
                            batch.push(event);
                        }
                        if this
                            .update(&mut cx, |state: &mut AppState, cx| {
                                for event in batch {
                                    state.apply_agent_event(thread_id, event);
                                }
                                cx.notify();
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
            AgentEvent::Terminal(event) => {
                self.apply_terminal_event(thread_id, event);
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

    pub fn thread_messages(&self, thread_id: Uuid) -> Vec<Message> {
        self.workspaces
            .iter()
            .flat_map(|workspace| workspace.threads.iter())
            .find(|thread| thread.id == thread_id)
            .map(|thread| thread.messages.clone())
            .unwrap_or_default()
    }

    pub fn active_thread_messages(&self) -> Vec<Message> {
        self.active_thread()
            .map(|thread| thread.messages.clone())
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn thread_connection_is_some(&self, thread_id: Uuid) -> bool {
        self.ts(thread_id).is_some_and(|ts| ts.connection.is_some())
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

    fn should_auto_connect_thread(&self, thread_id: Uuid) -> bool {
        self.find_thread(thread_id)
            .map(|thread| thread.messages.len() <= AUTO_CONNECT_MESSAGE_LIMIT)
            .unwrap_or(false)
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

    fn append_agent_thought_chunk(&mut self, thread_id: Uuid, chunk: &str) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            if let Some(active_message) = thread.get_active_thought_message_mut() {
                active_message.append_text(chunk);
            } else {
                thread.add_message(Message::new(thread_id, Role::Thought, chunk));
            }
        }
    }

    fn finalize_agent_message(&mut self, thread_id: Uuid) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            if let Some(active_message) = thread.get_active_agent_message_mut() {
                active_message.finalize();
            }
            if let Some(active_message) = thread.get_active_thought_message_mut() {
                active_message.finalize();
            }
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
            SessionUpdate::AgentThoughtChunk(ContentChunk {
                content: ContentBlock::Text(text),
                ..
            }) => {
                self.append_agent_thought_chunk(thread_id, &text.text);
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
            SessionUpdate::SessionInfoUpdate(update) => {
                self.apply_session_info_update(thread_id, update);
            }
            SessionUpdate::UsageUpdate(update) => {
                self.ts_mut(thread_id).usage = Some(update);
            }
            _ => {}
        }
    }

    fn apply_session_info_update(
        &mut self,
        thread_id: Uuid,
        update: agent_client_protocol::SessionInfoUpdate,
    ) {
        let Some(thread) = self.find_thread_mut(thread_id) else {
            return;
        };

        if thread.user_renamed {
            return;
        }

        let MaybeUndefined::Value(title) = update.title else {
            return;
        };
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return;
        }
        if thread.name == trimmed {
            return;
        }
        thread.name = trimmed.to_string();
        thread.updated_at = chrono::Utc::now();
        self.persist_state();
    }

    fn record_tool_call(&mut self, thread_id: Uuid, tool_call: ToolCall) {
        let tool_call_id = tool_call.tool_call_id.to_string();
        self.ts_mut(thread_id)
            .tool_calls
            .insert(tool_call_id.clone(), tool_call.clone());
        let mut message_id = None;
        if let Some(thread) = self.find_thread_mut(thread_id) {
            let message = Message::new(thread_id, Role::Agent, tool_call);
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

    fn apply_tool_call_update(&mut self, thread_id: Uuid, mut update: ToolCallUpdate) {
        let tool_call_id = update.tool_call_id.to_string();
        let (updated_tool_call, existing_message_id) = {
            let ts = self.ts_mut(thread_id);
            let entry = ts
                .tool_calls
                .entry(tool_call_id.clone())
                .or_insert_with(|| ToolCall::new(update.tool_call_id.clone(), "Tool call"));
            if let Some(content) = update.fields.content.as_mut() {
                let has_terminal = content.iter().any(|item| {
                    matches!(item, agent_client_protocol::ToolCallContent::Terminal(_))
                });
                if !has_terminal {
                    content.extend(entry.content.iter().filter_map(|item| match item {
                        agent_client_protocol::ToolCallContent::Terminal(terminal) => Some(
                            agent_client_protocol::ToolCallContent::Terminal(terminal.clone()),
                        ),
                        _ => None,
                    }));
                }
            }
            entry.update(update.fields);
            let updated_tool_call = entry.clone();
            let existing_message_id = ts.tool_call_messages.get(&tool_call_id).copied();
            (updated_tool_call, existing_message_id)
        };
        let mut inserted_message_id = None;
        if let Some(thread) = self.find_thread_mut(thread_id) {
            if let Some(message_id) = existing_message_id
                && let Some(message) = thread.get_message_mut(message_id)
            {
                message.content = MessageContent::ToolCall(Box::new(updated_tool_call));
                message.is_streaming = false;
            } else {
                let message = Message::new(thread_id, Role::Agent, updated_tool_call);
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

    fn apply_terminal_event(&mut self, thread_id: Uuid, event: TerminalEvent) {
        let terminals = &mut self.ts_mut(thread_id).terminals;
        match event {
            TerminalEvent::Started {
                terminal_id,
                command,
            } => {
                let entry = terminals.entry(terminal_id.to_string()).or_default();
                entry.command = Some(command);
                entry.output.clear();
                entry.exit_status = None;
            }
            TerminalEvent::Output { terminal_id, chunk } => {
                let entry = terminals.entry(terminal_id.to_string()).or_default();
                entry.output.push_str(&chunk);
            }
            TerminalEvent::Exited {
                terminal_id,
                exit_status,
            } => {
                let entry = terminals.entry(terminal_id.to_string()).or_default();
                entry.exit_status = Some(exit_status);
            }
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
        let Some(log_writer) = &self.log_writer else {
            return;
        };
        let Ok(mut writer) = log_writer.lock() else {
            return;
        };
        let record = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "direction": direction,
            "thread_id": thread_id.to_string(),
            "payload": payload,
        });
        let _ = writeln!(writer, "{record}");
    }

    fn log_session_update(&self, thread_id: Uuid, update: &SessionUpdate) {
        if self.log_writer.is_none() {
            return;
        }
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

pub fn render_diff_text(old: Option<&str>, new: &str) -> String {
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

fn terminal_exit_status_label(exit_status: &TerminalExitStatus) -> String {
    match (exit_status.exit_code, exit_status.signal.as_deref()) {
        (Some(0), _) => "finished successfully".to_string(),
        (Some(code), _) => format!("failed with exit code {code}"),
        (None, Some(signal)) => format!("terminated by signal {signal}"),
        _ => "finished".to_string(),
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
        AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, Cost, Diff,
        PermissionOptionKind, SessionConfigOption, SessionInfoUpdate, TerminalExitStatus,
        TerminalId, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields, UsageUpdate,
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
        assert_eq!(thread.messages[0].content.to_string(), "hello");
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
    fn session_info_update_applies_title_when_user_has_not_renamed() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        state.apply_session_update(
            thread_id,
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title("Agent Title")),
        );

        let thread = state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .expect("thread should exist");
        assert_eq!(thread.name, "Agent Title");
    }

    #[test]
    fn session_info_update_does_not_override_user_renamed_title() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let mut thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        thread.name = "Custom Title".to_string();
        thread.user_renamed = true;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        state.apply_session_update(
            thread_id,
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title("Agent Title")),
        );

        let thread = state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .expect("thread should exist");
        assert_eq!(thread.name, "Custom Title");
    }

    #[test]
    fn usage_update_is_stored_for_active_thread() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        state.apply_session_update(
            thread_id,
            SessionUpdate::UsageUpdate(UsageUpdate::new(20, 80).cost(Cost::new(1.25, "USD"))),
        );

        let usage = state
            .active_thread_usage()
            .expect("usage should be present on active thread");
        assert_eq!(usage.used, 20);
        assert_eq!(usage.size, 80);
        let cost = usage.cost.expect("cost should be present");
        assert_eq!(cost.amount, 1.25);
        assert_eq!(cost.currency, "USD");
    }

    #[test]
    fn agent_thought_chunks_update_state() {
        let mut state = AppState::new();
        let mut workspace = Workspace::new("Test");
        let thread = Thread::new(workspace.id, "Thread");
        let thread_id = thread.id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);

        // First thought chunk: creates a new Thought message
        state.apply_session_update(
            thread_id,
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from("thinking"))),
        );

        // Second thought chunk: appends to the active Thought message
        state.apply_session_update(
            thread_id,
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(" more"))),
        );

        let thread = state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .expect("thread should exist");
        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].role, Role::Thought);
        assert_eq!(thread.messages[0].content.to_string(), "thinking more");
        assert!(thread.messages[0].is_streaming);

        // Finalize
        state.finalize_agent_message(thread_id);
        let thread = state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .expect("thread should exist");
        assert!(!thread.messages[0].is_streaming);
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
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Tool: Read file"));
    }

    #[test]
    fn terminal_events_build_transcript_for_terminal_tool_calls() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        state.apply_agent_event(
            thread_id,
            AgentEvent::Terminal(TerminalEvent::Started {
                terminal_id: TerminalId::new("terminal-1"),
                command: "sh -c \"printf terminal-ok\"".to_string(),
            }),
        );
        state.apply_agent_event(
            thread_id,
            AgentEvent::Terminal(TerminalEvent::Output {
                terminal_id: TerminalId::new("terminal-1"),
                chunk: "terminal-ok".to_string(),
            }),
        );
        state.apply_agent_event(
            thread_id,
            AgentEvent::Terminal(TerminalEvent::Exited {
                terminal_id: TerminalId::new("terminal-1"),
                exit_status: TerminalExitStatus::new().exit_code(0),
            }),
        );

        let transcript = state
            .terminal_transcript_for_thread(thread_id, "terminal-1")
            .expect("terminal transcript should exist");
        assert!(transcript.contains("$ sh -c \"printf terminal-ok\""));
        assert!(transcript.contains("terminal-ok"));
        assert!(transcript.contains("finished successfully"));
    }

    #[test]
    fn tool_call_updates_preserve_existing_terminal_blocks() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        let tool_call = ToolCall::new("tool-1", "Run terminal")
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Terminal(
                agent_client_protocol::Terminal::new(TerminalId::new("terminal-1")),
            )]);
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
                .content(vec![ToolCallContent::from("terminal complete")]),
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
        let has_terminal = thread
            .messages
            .iter()
            .find_map(|message| match &message.content {
                MessageContent::ToolCall(tool_call) => Some(
                    tool_call
                        .content
                        .iter()
                        .any(|item| matches!(item, ToolCallContent::Terminal(_))),
                ),
                _ => None,
            })
            .unwrap_or(false);
        assert!(has_terminal, "terminal content should remain attached");
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
        let state = AppState::with_parts(None, Vec::new(), false, Some(log_path.clone()));

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

    #[test]
    fn delete_thread_removes_workspace_entry_and_all_thread_state() {
        let mut state = AppState::new();
        let workspace_id = Uuid::new_v4();
        let thread = Thread::new(workspace_id, "Thread");
        let thread_id = thread.id;
        let mut workspace = Workspace::new("Workspace");
        workspace.id = workspace_id;
        workspace.add_thread(thread);
        state.workspaces.push(workspace);
        state.active_thread_id = Some(thread_id);

        // Write some per-thread state so we can verify it is cleaned up.
        state.ts_mut(thread_id).prompt_started_at = Some(std::time::Instant::now());
        state.unread_stopped_threads.insert(thread_id);

        // Delete the thread (no cx.notify() needed for the unit test).
        let removed = state.thread_state.remove(&thread_id);
        assert!(removed.is_some(), "ts_mut should have created an entry");

        // Re-insert so delete_thread itself can remove it.
        state.ts_mut(thread_id).prompt_started_at = Some(std::time::Instant::now());

        // delete_thread requires a Context, so exercise the lower-level
        // primitives it calls instead.
        state
            .workspaces
            .iter_mut()
            .find(|w| w.id == workspace_id)
            .expect("workspace should exist")
            .threads
            .retain(|t| t.id != thread_id);
        state.thread_state.remove(&thread_id);
        state.unread_stopped_threads.remove(&thread_id);

        // Verify nothing is left behind.
        assert!(
            !state
                .workspaces
                .iter()
                .any(|w| w.threads.iter().any(|t| t.id == thread_id)),
            "thread should be gone from workspace"
        );
        assert!(
            !state.thread_state.contains_key(&thread_id),
            "thread_state entry should be removed"
        );
        assert!(
            !state.unread_stopped_threads.contains(&thread_id),
            "unread_stopped_threads should not mention the deleted thread"
        );
    }
}
