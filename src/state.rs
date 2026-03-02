use gpui::{Context, Task};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::{AcpController, AgentEvent, PermissionRequestEvent};
use crate::config::AppConfig;
use crate::domain::{Message, Role, Thread, Workspace};
use crate::persistence::AppPersistence;
use agent_client_protocol::{
    AvailableCommand, ContentBlock, ContentChunk, PermissionOption, PermissionOptionId,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionConfigId, SessionConfigOption,
    SessionConfigValueId, SessionId, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate,
};
use ignore::WalkBuilder;

struct ThreadAgentConnection {
    controller: Rc<AcpController>,
    session_id: SessionId,
    _process: Option<std::process::Child>,
}

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_thread_id: Option<Uuid>,
    agent_tasks: HashMap<Uuid, Task<()>>,
    mock_event_senders: HashMap<Uuid, mpsc::UnboundedSender<AgentEvent>>,
    agent_connections: HashMap<Uuid, ThreadAgentConnection>,
    pending_permissions: HashMap<Uuid, PermissionRequestEvent>,
    thread_config_options: HashMap<Uuid, Vec<SessionConfigOption>>,
    thread_available_commands: HashMap<Uuid, Vec<AvailableCommand>>,
    thread_tool_calls: HashMap<Uuid, HashMap<String, ToolCall>>,
    persistence: Option<AppPersistence>,
    agent_config_path: Option<PathBuf>,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_parts(None, None)
    }

    pub fn new_with_config(config: AppConfig) -> Self {
        Self::with_parts(
            Some(AppPersistence::new(config.data_dir)),
            config.agent_config,
        )
    }

    fn with_parts(persistence: Option<AppPersistence>, agent_config_path: Option<PathBuf>) -> Self {
        Self {
            workspaces: Vec::new(),
            active_thread_id: None,
            agent_tasks: HashMap::new(),
            mock_event_senders: HashMap::new(),
            agent_connections: HashMap::new(),
            pending_permissions: HashMap::new(),
            thread_config_options: HashMap::new(),
            thread_available_commands: HashMap::new(),
            thread_tool_calls: HashMap::new(),
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
        self.mock_event_senders.insert(thread_id, tx);

        if let Some(config_path) = self.agent_config_path.clone() {
            self.connect_thread_to_agent_config(cx, thread_id, config_path);
        }

        self.persist_state();
        cx.notify();
        Some(thread_id)
    }

    pub fn set_active_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        self.active_thread_id = Some(thread_id);
        if !self.agent_connections.contains_key(&thread_id)
            && let Some(config_path) = self.agent_config_path.clone()
        {
            self.connect_thread_to_agent_config(cx, thread_id, config_path);
        }
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
        self.pending_permissions
            .get(&thread_id)
            .map(|request| request.options.clone())
    }

    pub fn active_thread_config_options(&self) -> Option<Vec<SessionConfigOption>> {
        let thread_id = self.active_thread_id?;
        self.thread_config_options.get(&thread_id).cloned()
    }

    pub fn active_thread_available_commands(&self) -> Option<Vec<AvailableCommand>> {
        let thread_id = self.active_thread_id?;
        self.thread_available_commands.get(&thread_id).cloned()
    }

    pub fn resolve_permission(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        option_id: Option<String>,
    ) {
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
        let Some(connection) = self.agent_connections.get(&thread_id) else {
            return;
        };
        let controller = Rc::clone(&connection.controller);
        let session_id = connection.session_id.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
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
                                state
                                    .thread_config_options
                                    .insert(thread_id, config_options);
                                cx.notify();
                            });
                        }
                        Err(err) => {
                            let message = format!("Failed to set session option: {err}");
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

        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::User, content));
        }

        if let Some(connection) = self.agent_connections.get(&thread_id) {
            let controller = Rc::clone(&connection.controller);
            let session_id = connection.session_id.clone();
            let content = content.to_owned();
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
                                    state.push_system_message(cx, thread_id, message);
                                });
                            }
                        }
                    }
                },
            )
            .detach();
        } else if let Some(event_tx) = self.mock_event_senders.get(&thread_id).cloned() {
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
        cx.spawn(
            move |this: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let result = AcpController::connect_from_config(config_path, event_tx).await;

                    match result {
                        Ok((controller, process)) => {
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
                                match controller
                                    .load_session(session_id.clone(), cwd.clone())
                                    .await
                                {
                                    Ok(config_options) => Some((session_id, config_options)),
                                    Err(err) => {
                                        let message = format!("Failed to load ACP session: {err}");
                                        let _ = this.update(&mut cx, |state: &mut AppState, cx| {
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
                                controller.initialize_session(cwd).await
                            };

                            match session_result {
                                Ok((session_id, config_options)) => {
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.attach_connection(
                                            cx,
                                            thread_id,
                                            controller,
                                            session_id,
                                            config_options,
                                            Some(process),
                                            event_rx,
                                        );
                                    });
                                }
                                Err(err) => {
                                    let message =
                                        format!("Failed to initialize ACP session: {err}");
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.push_system_message(cx, thread_id, message);
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            let message = format!("Failed to connect ACP controller: {err}");
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

    fn attach_connection(
        &mut self,
        cx: &mut Context<Self>,
        thread_id: Uuid,
        controller: AcpController,
        session_id: SessionId,
        config_options: Vec<SessionConfigOption>,
        process: Option<std::process::Child>,
        rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        self.listen_to_agent_events(cx, thread_id, rx);
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.session_id = Some(session_id.to_string());
        }
        self.agent_connections.insert(
            thread_id,
            ThreadAgentConnection {
                controller: Rc::new(controller),
                session_id,
                _process: process,
            },
        );
        self.thread_config_options.insert(thread_id, config_options);
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

        self.agent_tasks.insert(thread_id, task);
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
                if let Some(existing) = self.pending_permissions.remove(&thread_id) {
                    let _ = existing
                        .response_tx
                        .send(RequestPermissionOutcome::Cancelled);
                }
                self.pending_permissions.insert(thread_id, request);
            }
            AgentEvent::Disconnected => {
                self.finalize_agent_message(thread_id);
            }
        }
    }

    pub fn active_thread_messages(&self) -> Vec<Message> {
        let Some(thread_id) = self.active_thread_id else {
            return Vec::new();
        };

        self.workspaces
            .iter()
            .find_map(|workspace| workspace.get_thread(thread_id))
            .map(|thread| thread.messages.clone())
            .unwrap_or_default()
    }

    pub fn active_thread_message_count(&self) -> usize {
        self.active_thread_messages().len()
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
        match update {
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(text),
                ..
            }) => {
                self.append_agent_chunk(thread_id, &text.text);
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.thread_config_options
                    .insert(thread_id, update.config_options);
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.thread_available_commands
                    .insert(thread_id, update.available_commands);
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.record_tool_call(thread_id, tool_call);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.apply_tool_call_update(thread_id, update);
            }
            _ => {}
        }
    }

    fn record_tool_call(&mut self, thread_id: Uuid, tool_call: ToolCall) {
        self.thread_tool_calls
            .entry(thread_id)
            .or_default()
            .insert(tool_call.tool_call_id.to_string(), tool_call.clone());
        let rendered = render_tool_call_message(&tool_call);
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::Agent, rendered));
            if let Some(active_message) = thread.get_active_agent_message_mut() {
                active_message.finalize();
            }
        }
    }

    fn apply_tool_call_update(&mut self, thread_id: Uuid, update: ToolCallUpdate) {
        let entry = self
            .thread_tool_calls
            .entry(thread_id)
            .or_default()
            .entry(update.tool_call_id.to_string())
            .or_insert_with(|| ToolCall::new(update.tool_call_id.clone(), "Tool call"));
        entry.update(update.fields);
        let rendered = render_tool_call_message(entry);
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::Agent, rendered));
            if let Some(active_message) = thread.get_active_agent_message_mut() {
                active_message.finalize();
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
        let message = format!("Stop reason: {}", stop_reason_label(stop_reason));
        self.push_system_message(cx, thread_id, message);
    }

    fn push_system_message(&mut self, cx: &mut Context<Self>, thread_id: Uuid, content: String) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::System, content));
            cx.notify();
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
        let Some(request) = self.pending_permissions.remove(&thread_id) else {
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

fn render_tool_call_content(content: &ToolCallContent) -> String {
    match content {
        ToolCallContent::Content(content) => match &content.content {
            ContentBlock::Text(text) => text.text.clone(),
            _ => "[non-text content]".to_string(),
        },
        ToolCallContent::Diff(diff) => {
            let mut out = vec![format!("Diff: {}", diff.path.display())];
            out.push(render_diff_text(diff.old_text.as_deref(), &diff.new_text));
            out.join("\n")
        }
        ToolCallContent::Terminal(terminal) => {
            format!("Terminal: {}", terminal.terminal_id)
        }
        _ => "[unsupported tool content]".to_string(),
    }
}

fn render_diff_text(old: Option<&str>, new: &str) -> String {
    let mut lines = vec!["--- before".to_string(), "+++ after".to_string()];
    let old_lines = old.map(|text| text.lines().collect::<Vec<_>>());
    let new_lines = new.lines().collect::<Vec<_>>();
    let max_len = old_lines
        .as_ref()
        .map_or(new_lines.len(), |values| values.len().max(new_lines.len()));
    for index in 0..max_len {
        let before = old_lines
            .as_ref()
            .and_then(|values| values.get(index))
            .copied();
        let after = new_lines.get(index).copied();
        if before == after {
            continue;
        }
        if let Some(before) = before {
            lines.push(format!("-{before}"));
        }
        if let Some(after) = after {
            lines.push(format!("+{after}"));
        }
    }
    if lines.len() == 2 {
        lines.push("(no changes)".to_string());
    }
    lines.join("\n")
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
        let rendered = thread
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Tool: Read file"));
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
}
