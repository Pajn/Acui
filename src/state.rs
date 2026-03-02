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
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionId, RequestPermissionOutcome,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
};

struct ThreadAgentConnection {
    controller: Rc<AcpController>,
    session_id: SessionId,
    _process: Option<tokio::process::Child>,
}

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_thread_id: Option<Uuid>,
    agent_tasks: HashMap<Uuid, Task<()>>,
    mock_event_senders: HashMap<Uuid, mpsc::UnboundedSender<AgentEvent>>,
    agent_connections: HashMap<Uuid, ThreadAgentConnection>,
    pending_permissions: HashMap<Uuid, PermissionRequestEvent>,
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

    pub fn active_thread_permission_options(&self) -> Option<Vec<PermissionOption>> {
        let thread_id = self.active_thread_id?;
        self.pending_permissions
            .get(&thread_id)
            .map(|request| request.options.clone())
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
                                    Ok(()) => Some(session_id),
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

                            let session_result = if let Some(session_id) = loaded_session_id {
                                Ok(session_id)
                            } else {
                                controller.initialize_session(cwd).await
                            };

                            match session_result {
                                Ok(session_id) => {
                                    let _ = this.update(&mut cx, |state: &mut AppState, cx| {
                                        state.attach_connection(
                                            cx,
                                            thread_id,
                                            controller,
                                            session_id,
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
        process: Option<tokio::process::Child>,
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
                if let Some(chunk) = extract_text_from_notification(&notification) {
                    self.append_agent_chunk(thread_id, &chunk);
                }
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
        if let Some(thread) = self.find_thread_mut(thread_id) {
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
    use agent_client_protocol::PermissionOptionKind;

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
}
