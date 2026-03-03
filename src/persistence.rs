use crate::domain::{Message, MessageContent, Role, Thread, Workspace};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct AppPersistence {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceRecord {
    id: Uuid,
    name: String,
    path: PathBuf,
    created_at: DateTime<Utc>,
    thread_ids: Vec<Uuid>,
}

/// Compact per-message record stored inside `ThreadRecord`.
#[derive(Debug, Serialize, Deserialize)]
struct MessageRecord {
    id: Uuid,
    role: Role,
    content: MessageContent,
    timestamp: DateTime<Utc>,
}

impl MessageRecord {
    fn from_message(m: &Message) -> Self {
        Self {
            id: m.id,
            role: m.role,
            content: m.content.clone(),
            timestamp: m.timestamp,
        }
    }

    fn into_message(self, thread_id: Uuid) -> Message {
        Message {
            id: self.id,
            thread_id,
            role: self.role,
            content: self.content,
            timestamp: self.timestamp,
            is_streaming: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ThreadRecord {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    #[serde(default)]
    agent_name: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    messages: Vec<MessageRecord>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl AppPersistence {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn load(&self) -> anyhow::Result<Vec<Workspace>> {
        let workspace_records = read_records::<WorkspaceRecord>(&self.workspaces_dir())?;
        let thread_records = read_records::<ThreadRecord>(&self.threads_dir())?;

        let mut workspaces = workspace_records
            .into_iter()
            .map(|workspace| Workspace {
                id: workspace.id,
                name: workspace.name,
                path: workspace.path,
                threads: Vec::new(),
                created_at: workspace.created_at,
            })
            .collect::<Vec<_>>();

        for thread in thread_records {
            if let Some(workspace) = workspaces.iter_mut().find(|w| w.id == thread.workspace_id) {
                let thread_id = thread.id;
                workspace.threads.push(Thread {
                    id: thread_id,
                    workspace_id: thread.workspace_id,
                    name: thread.name,
                    agent_name: thread.agent_name,
                    session_id: thread.session_id,
                    messages: thread
                        .messages
                        .into_iter()
                        .map(|m| m.into_message(thread_id))
                        .collect(),
                    created_at: thread.created_at,
                    updated_at: thread.updated_at,
                });
            }
        }

        workspaces.sort_by_key(|workspace| workspace.created_at);
        for workspace in &mut workspaces {
            workspace.threads.sort_by_key(|thread| thread.created_at);
        }
        Ok(workspaces)
    }

    pub fn save(&self, workspaces: &[Workspace]) -> anyhow::Result<()> {
        self.prepare_dirs()?;
        let mut workspace_files = HashSet::new();
        let mut thread_files = HashSet::new();
        for workspace in workspaces {
            let workspace_file = format!("{}.json", workspace.id);
            workspace_files.insert(workspace_file.clone());
            let workspace_record = WorkspaceRecord {
                id: workspace.id,
                name: workspace.name.clone(),
                path: workspace.path.clone(),
                created_at: workspace.created_at,
                thread_ids: workspace.threads.iter().map(|thread| thread.id).collect(),
            };
            write_record(
                &self.workspaces_dir().join(workspace_file),
                &workspace_record,
            )?;

            for thread in &workspace.threads {
                let thread_file = format!("{}.json", thread.id);
                thread_files.insert(thread_file.clone());
                let thread_record = ThreadRecord {
                    id: thread.id,
                    workspace_id: thread.workspace_id,
                    name: thread.name.clone(),
                    agent_name: thread.agent_name.clone(),
                    session_id: thread.session_id.clone(),
                    messages: thread
                        .messages
                        .iter()
                        .filter(|m| !m.is_streaming)
                        .map(MessageRecord::from_message)
                        .collect(),
                    created_at: thread.created_at,
                    updated_at: thread.updated_at,
                };
                write_record(&self.threads_dir().join(thread_file), &thread_record)?;
            }
        }

        cleanup_stale_records(&self.workspaces_dir(), &workspace_files)?;
        cleanup_stale_records(&self.threads_dir(), &thread_files)?;
        Ok(())
    }

    fn prepare_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.workspaces_dir())?;
        std::fs::create_dir_all(self.threads_dir())?;
        Ok(())
    }

    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    fn threads_dir(&self) -> PathBuf {
        self.root.join("threads")
    }
}

fn read_records<T>(dir: &Path) -> anyhow::Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(path)?;
        records.push(serde_json::from_str(&raw)?);
    }
    Ok(records)
}

fn write_record<T: Serialize>(path: &Path, record: &T) -> anyhow::Result<()> {
    let serialized = serde_json::to_string_pretty(record)?;
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, serialized)?;
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err.into());
    }
    Ok(())
}

fn cleanup_stale_records(dir: &Path, keep_files: &HashSet<String>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(file_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if !keep_files.contains(&file_name) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AppPersistence;
    use crate::domain::{Message, MessageContent, Role, Thread, Workspace};
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn persistence_round_trip_keeps_workspace_thread_and_session() {
        let root = std::env::temp_dir().join(format!("acui-persist-test-{}", Uuid::new_v4()));
        let persistence = AppPersistence::new(root.clone());

        let mut workspace = Workspace::from_path(PathBuf::from("/tmp/my-workspace"));
        let mut thread = Thread::new(workspace.id, "Thread 1");
        thread.session_id = Some("session-123".to_string());
        workspace.add_thread(thread);

        persistence
            .save(&[workspace.clone()])
            .expect("save should succeed");
        let loaded = persistence.load().expect("load should succeed");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, PathBuf::from("/tmp/my-workspace"));
        assert_eq!(loaded[0].threads.len(), 1);
        assert_eq!(
            loaded[0].threads[0].session_id.as_deref(),
            Some("session-123")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn messages_survive_persist_round_trip() {
        let root = std::env::temp_dir().join(format!("acui-persist-test-{}", Uuid::new_v4()));
        let persistence = AppPersistence::new(root.clone());

        let mut workspace = Workspace::from_path(PathBuf::from("/tmp/msg-workspace"));
        let thread_id = Uuid::new_v4();
        let mut thread = Thread::new(workspace.id, "Thread With Messages");
        thread.id = thread_id;
        thread.messages.push(Message {
            id: Uuid::new_v4(),
            thread_id,
            role: Role::User,
            content: MessageContent::from("Hello agent"),
            timestamp: chrono::Utc::now(),
            is_streaming: false,
        });
        thread.messages.push(Message {
            id: Uuid::new_v4(),
            thread_id,
            role: Role::Agent,
            content: MessageContent::from("Hello user"),
            timestamp: chrono::Utc::now(),
            is_streaming: false,
        });
        // Streaming messages should not be persisted.
        thread.messages.push(Message {
            id: Uuid::new_v4(),
            thread_id,
            role: Role::Agent,
            content: MessageContent::from("still typing…"),
            timestamp: chrono::Utc::now(),
            is_streaming: true,
        });
        workspace.add_thread(thread);

        persistence.save(&[workspace]).expect("save should succeed");
        let loaded = persistence.load().expect("load should succeed");

        let msgs = &loaded[0].threads[0].messages;
        assert_eq!(msgs.len(), 2, "streaming message must not be persisted");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.to_string(), "Hello agent");
        assert_eq!(msgs[1].role, Role::Agent);
        assert!(
            !msgs[1].is_streaming,
            "loaded messages must not be streaming"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_does_not_delete_non_record_files_in_persistence_dirs() {
        let root = std::env::temp_dir().join(format!("acui-persist-test-{}", Uuid::new_v4()));
        let persistence = AppPersistence::new(root.clone());

        let workspace = Workspace::from_path(PathBuf::from("/tmp/my-workspace"));
        persistence
            .save(&[workspace])
            .expect("initial save should succeed");

        let marker = root.join("workspaces").join("marker.txt");
        std::fs::write(&marker, "keep me").expect("should write marker");

        persistence
            .save(&[])
            .expect("second save should succeed without deleting marker");

        assert!(marker.exists(), "marker file should remain after save");
        let _ = std::fs::remove_dir_all(root);
    }
}
