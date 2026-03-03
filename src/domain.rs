use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Represents who sent the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Agent,
    Thought, // Dimmed thought chunks from agent
    System,  // Useful for connection errors or app-level notifications
}

/// A UI-level representation of a message.
/// We use this to aggregate the streaming chunks sent from the ACP connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    /// True if the agent is currently streaming this message via ACP
    pub is_streaming: bool,
}

impl Message {
    pub fn new(thread_id: Uuid, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            thread_id,
            role,
            content: content.into(),
            timestamp: Utc::now(),
            is_streaming: role == Role::Agent || role == Role::Thought,
        }
    }

    /// Folds incoming ACP text chunks into this message.
    pub fn append_text(&mut self, chunk: &str) {
        self.content.push_str(chunk);
    }

    /// Marks the message as complete when the ACP stop reason is received.
    pub fn finalize(&mut self) {
        self.is_streaming = false;
    }
}

/// A conversation thread containing a sequence of messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    /// The agent this thread is locked to after the first message is sent.
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Thread {
    pub fn new(workspace_id: Uuid, name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            workspace_id,
            name: name.into(),
            agent_name: None,
            session_id: None,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
        self.updated_at = Utc::now();
    }

    /// Retrieves the active streaming message.
    /// The AppState uses this to know where to append incoming ACP chunks.
    pub fn get_active_agent_message_mut(&mut self) -> Option<&mut Message> {
        self.messages
            .last_mut()
            .filter(|m| m.role == Role::Agent && m.is_streaming)
    }

    /// Retrieves the active streaming thought message.
    pub fn get_active_thought_message_mut(&mut self) -> Option<&mut Message> {
        self.messages
            .last_mut()
            .filter(|m| m.role == Role::Thought && m.is_streaming)
    }

    pub fn get_message_mut(&mut self, message_id: Uuid) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .find(|message| message.id == message_id)
    }
}

/// A workspace acting as a folder for multiple threads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub threads: Vec<Thread>,
    pub created_at: DateTime<Utc>,
}

impl Workspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            path: PathBuf::from("."),
            threads: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn from_path(path: PathBuf) -> Self {
        let fallback = path.display().to_string();
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .map_or(fallback, ToOwned::to_owned);
        Self {
            id: Uuid::new_v4(),
            name,
            path,
            threads: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn add_thread(&mut self, thread: Thread) {
        self.threads.push(thread);
    }

    pub fn get_thread_mut(&mut self, thread_id: Uuid) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == thread_id)
    }

    pub fn get_thread(&self, thread_id: Uuid) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::Workspace;
    use std::path::PathBuf;

    #[test]
    fn workspace_name_uses_directory_name() {
        let workspace = Workspace::from_path(PathBuf::from("/tmp/acui-project"));
        assert_eq!(workspace.name, "acui-project");
    }
}
