use gpui::{Context, Task};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::{AcpController, AgentEvent};
use crate::domain::{Message, Role, Thread, Workspace};
use agent_client_protocol::{
    ContentBlock, ContentChunk, SessionId, SessionNotification, SessionUpdate,
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
}

impl AppState {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active_thread_id: None,
            agent_tasks: HashMap::new(),
            mock_event_senders: HashMap::new(),
            agent_connections: HashMap::new(),
        }
    }

    pub fn add_workspace(&mut self, cx: &mut Context<Self>, name: &str) -> Uuid {
        let workspace = Workspace::new(name);
        let id = workspace.id;
        self.workspaces.push(workspace);
        cx.notify();
        id
    }

    pub fn add_workspace_from_path(&mut self, cx: &mut Context<Self>, path: PathBuf) -> Uuid {
        let workspace = Workspace::from_path(path);
        let id = workspace.id;
        self.workspaces.push(workspace);
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

        cx.notify();
        Some(thread_id)
    }

    pub fn set_active_thread(&mut self, cx: &mut Context<Self>, thread_id: Uuid) {
        self.active_thread_id = Some(thread_id);
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
                move |_this: gpui::WeakEntity<AppState>, _cx: &mut gpui::AsyncApp| async move {
                    let _ = controller.send_prompt(session_id, content).await;
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
                            let cwd = this
                                .read_with(&cx, |state, _| {
                                    state.workspace_cwd_for_thread(thread_id)
                                })
                                .ok()
                                .flatten()
                                .unwrap_or_else(|| {
                                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                                });
                            match controller.initialize_session(cwd).await {
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
        self.agent_connections.insert(
            thread_id,
            ThreadAgentConnection {
                controller: Rc::new(controller),
                session_id,
                _process: process,
            },
        );
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

    fn push_system_message(&mut self, cx: &mut Context<Self>, thread_id: Uuid, content: String) {
        if let Some(thread) = self.find_thread_mut(thread_id) {
            thread.add_message(Message::new(thread_id, Role::System, content));
            cx.notify();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
