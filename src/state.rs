use gpui::{Context, Task};
use std::collections::HashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::AgentEvent;
use crate::domain::{Message, Role, Thread, Workspace};
// Note to Agent: Import the correct types from agent_client_protocol based on version
use agent_client_protocol::SessionNotification;

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_thread_id: Option<Uuid>,
    // Store background tasks tied to specific threads so they aren't dropped.
    // Dropping a Task in GPUI cancels the underlying async operation.
    agent_tasks: HashMap<Uuid, Task<()>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active_thread_id: None,
            agent_tasks: HashMap::new(),
        }
    }

    pub fn add_workspace(&mut self, cx: &mut Context<Self>, name: &str) -> Uuid {
        let workspace = Workspace::new(name);
        let id = workspace.id;
        self.workspaces.push(workspace);
        cx.notify();
        id
    }

    pub fn add_thread(
        &mut self,
        cx: &mut ModelContext<Self>,
        workspace_id: Uuid,
        name: &str,
    ) -> Option<Uuid> {
        let workspace = self.workspaces.iter_mut().find(|w| w.id == workspace_id)?;
        let thread = Thread::new(workspace_id, name);
        let thread_id = thread.id;
        workspace.add_thread(thread);
        self.active_thread_id = Some(thread_id);
        cx.notify();
        Some(thread_id)
    }

    pub fn set_active_thread(&mut self, cx: &mut ModelContext<Self>, thread_id: Uuid) {
        self.active_thread_id = Some(thread_id);
        cx.notify();
    }

    /// Adds a user message to the thread.
    /// (In Phase 4, this is also where you will use the AcpController to send the PromptRequest to the agent).
    pub fn send_user_message(
        &mut self,
        cx: &mut ModelContext<Self>,
        thread_id: Uuid,
        content: &str,
    ) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|w| w.threads.iter().any(|t| t.id == thread_id))
        {
            if let Some(thread) = workspace.get_thread_mut(thread_id) {
                thread.add_message(Message::new(thread_id, Role::User, content));
                cx.notify();
            }
        }
    }

    /// Spawns a background task to listen to the agent's stream for a specific thread.
    /// This bridges Tokio and GPUI.
    pub fn listen_to_agent_events(
        &mut self,
        cx: &mut ModelContext<Self>,
        thread_id: Uuid,
        mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        // cx.spawn allows us to run async code that can safely mutate the GPUI Model
        let task = cx.spawn(|this, mut cx| async move {
            while let Some(event) = rx.recv().await {
                // When an event arrives, use `this.update` to safely mutate the AppState
                let result = this.update(&mut cx, |state, cx| {
                    match event {
                        AgentEvent::Notification(notification) => {
                            // NOTE TO AGENT: Parse the specific SessionNotification from the ACP crate here.
                            // For this example, we assume we extract a `text_chunk` string.
                            let text_chunk = extract_text_from_notification(&notification);

                            if let Some(chunk) = text_chunk {
                                state.append_agent_chunk(cx, thread_id, &chunk);
                            }
                        }
                        AgentEvent::Disconnected => {
                            state.finalize_agent_message(cx, thread_id);
                        }
                    }
                });

                if result.is_err() {
                    // The model was dropped, meaning the app is shutting down.
                    break;
                }
            }
        });

        self.agent_tasks.insert(thread_id, task);
    }

    /// Helper to route incoming text chunks to the correct message bubble
    fn append_agent_chunk(&mut self, cx: &mut ModelContext<Self>, thread_id: Uuid, chunk: &str) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|w| w.threads.iter().any(|t| t.id == thread_id))
        {
            if let Some(thread) = workspace.get_thread_mut(thread_id) {
                if let Some(active_message) = thread.get_active_agent_message_mut() {
                    // Append to existing streaming message
                    active_message.append_text(chunk);
                } else {
                    // Create a new streaming message bubble
                    thread.add_message(Message::new(thread_id, Role::Agent, chunk));
                }
                cx.notify(); // Tell GPUI to redraw the chat view!
            }
        }
    }

    fn finalize_agent_message(&mut self, cx: &mut ModelContext<Self>, thread_id: Uuid) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|w| w.threads.iter().any(|t| t.id == thread_id))
        {
            if let Some(thread) = workspace.get_thread_mut(thread_id) {
                if let Some(active_message) = thread.get_active_agent_message_mut() {
                    active_message.finalize();
                    cx.notify();
                }
            }
        }
    }
}

// Dummy helper for the agent to replace with actual ACP parsing logic
fn extract_text_from_notification(_notification: &SessionNotification) -> Option<String> {
    // Implement parsing based on agent_client_protocol schema
    Some("...".to_string())
}
