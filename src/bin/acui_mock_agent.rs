use agent_client_protocol::{
    self as acp, Agent, AgentSideConnection, AuthenticateRequest, AuthenticateResponse,
    AvailableCommand, AvailableCommandsUpdate, CancelNotification, Client, ContentBlock,
    ContentChunk, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, ReadTextFileRequest,
    RequestPermissionOutcome, RequestPermissionRequest, SessionConfigOption,
    SessionConfigSelectOption, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use async_trait::async_trait;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

struct MockAgent {
    client: Rc<RefCell<Option<Rc<AgentSideConnection>>>>,
    cwd: PathBuf,
}

impl MockAgent {
    fn client(&self) -> acp::Result<Rc<AgentSideConnection>> {
        self.client
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(acp::Error::internal_error)
    }

    async fn send_text(
        &self,
        session_id: &acp::SessionId,
        text: impl Into<String>,
    ) -> acp::Result<()> {
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                    text.into(),
                ))),
            ))
            .await
    }

    async fn send_available_commands(&self, session_id: &acp::SessionId) -> acp::Result<()> {
        let commands = vec![
            AvailableCommand::new("cwd", "Return the mock agent current working directory"),
            AvailableCommand::new("permission", "Trigger a permission request"),
            AvailableCommand::new("terminal", "Run a terminal command and report output"),
            AvailableCommand::new("read", "Read a file (usage: read <path>)"),
            AvailableCommand::new("write", "Write a file (usage: write <path> <content>)"),
        ];
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)),
            ))
            .await
    }

    async fn handle_permission_prompt(&self, session_id: &acp::SessionId) -> acp::Result<()> {
        let tool_call =
            ToolCall::new("permission-call", "Request permission").status(ToolCallStatus::Pending);
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCall(tool_call.clone()),
            ))
            .await?;

        let response = self
            .client()?
            .request_permission(RequestPermissionRequest::new(
                session_id.clone(),
                tool_call.into(),
                vec![
                    PermissionOption::new(
                        "allow_once",
                        "Allow once",
                        PermissionOptionKind::AllowOnce,
                    ),
                    PermissionOption::new(
                        "reject_once",
                        "Reject once",
                        PermissionOptionKind::RejectOnce,
                    ),
                ],
            ))
            .await?;

        let outcome = match response.outcome {
            RequestPermissionOutcome::Cancelled => "cancelled".to_string(),
            RequestPermissionOutcome::Selected(selected) => selected.option_id.to_string(),
            _ => "unknown".to_string(),
        };
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    "permission-call",
                    ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                )),
            ))
            .await?;
        self.send_text(session_id, format!("permission outcome: {outcome}"))
            .await
    }

    async fn handle_terminal_prompt(&self, session_id: &acp::SessionId) -> acp::Result<()> {
        let terminal = self
            .client()?
            .create_terminal(
                acp::CreateTerminalRequest::new(session_id.clone(), "sh")
                    .args(vec!["-c".to_string(), "printf mock-terminal".to_string()]),
            )
            .await?;
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCall(
                    ToolCall::new("terminal-call", "Run terminal")
                        .kind(ToolKind::Execute)
                        .status(ToolCallStatus::InProgress)
                        .content(vec![ToolCallContent::Terminal(acp::Terminal::new(
                            terminal.terminal_id.clone(),
                        ))]),
                ),
            ))
            .await?;

        let _ = self
            .client()?
            .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal.terminal_id.clone(),
            ))
            .await?;
        let output = self
            .client()?
            .terminal_output(acp::TerminalOutputRequest::new(
                session_id.clone(),
                terminal.terminal_id.clone(),
            ))
            .await?;
        self.client()?
            .session_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    "terminal-call",
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Completed)
                        .content(vec![ToolCallContent::from(format!(
                            "Terminal output:\n{}",
                            output.output
                        ))]),
                )),
            ))
            .await?;
        self.client()?
            .release_terminal(acp::ReleaseTerminalRequest::new(
                session_id.clone(),
                terminal.terminal_id,
            ))
            .await?;
        self.send_text(session_id, "terminal complete").await
    }

    async fn handle_read_prompt(&self, session_id: &acp::SessionId, path: &str) -> acp::Result<()> {
        let path = absolute_path(&self.cwd, path);
        let response = self
            .client()?
            .read_text_file(ReadTextFileRequest::new(session_id.clone(), path.clone()))
            .await?;
        self.send_text(
            session_id,
            format!(
                "read {} bytes from {}",
                response.content.len(),
                path.display()
            ),
        )
        .await
    }

    async fn handle_write_prompt(
        &self,
        session_id: &acp::SessionId,
        path: &str,
        content: &str,
    ) -> acp::Result<()> {
        let path = absolute_path(&self.cwd, path);
        self.client()?
            .write_text_file(WriteTextFileRequest::new(
                session_id.clone(),
                path.clone(),
                content.to_string(),
            ))
            .await?;
        self.send_text(session_id, format!("wrote {}", path.display()))
            .await
    }
}

#[async_trait(?Send)]
impl Agent for MockAgent {
    async fn initialize(&self, args: InitializeRequest) -> acp::Result<InitializeResponse> {
        Ok(InitializeResponse::new(args.protocol_version)
            .agent_info(Implementation::new("acui-mock-agent", "0.1.0")))
    }

    async fn authenticate(&self, _args: AuthenticateRequest) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, _args: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        let session_id = acp::SessionId::new(format!("mock-session-{}", uuid::Uuid::new_v4()));
        self.send_available_commands(&session_id).await?;
        Ok(NewSessionResponse::new(session_id).config_options(session_config_options("new")))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> acp::Result<LoadSessionResponse> {
        self.send_available_commands(&args.session_id).await?;
        Ok(LoadSessionResponse::new().config_options(session_config_options("loaded")))
    }

    async fn prompt(&self, args: PromptRequest) -> acp::Result<PromptResponse> {
        let prompt = prompt_text(&args.prompt);
        let trimmed = prompt.trim();
        if trimmed == "cwd" {
            self.send_text(&args.session_id, format!("cwd: {}", self.cwd.display()))
                .await?;
        } else if trimmed == "permission" {
            self.handle_permission_prompt(&args.session_id).await?;
        } else if trimmed == "terminal" {
            self.handle_terminal_prompt(&args.session_id).await?;
        } else if let Some(path) = trimmed.strip_prefix("read ").map(str::trim) {
            self.handle_read_prompt(&args.session_id, path).await?;
        } else if let Some(rest) = trimmed.strip_prefix("write ").map(str::trim) {
            let mut parts = rest.splitn(2, ' ');
            let path = parts.next().unwrap_or_default();
            let content = parts.next().unwrap_or_default();
            if path.is_empty() {
                self.send_text(&args.session_id, "usage: write <path> <content>")
                    .await?;
            } else {
                self.handle_write_prompt(&args.session_id, path, content)
                    .await?;
            }
        } else {
            self.send_text(&args.session_id, format!("echo: {trimmed}"))
                .await?;
        }

        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, _args: CancelNotification) -> acp::Result<()> {
        Ok(())
    }
}

fn prompt_text(prompt: &[ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn absolute_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn session_config_options(current: &str) -> Vec<SessionConfigOption> {
    vec![SessionConfigOption::select(
        "mode",
        "Mode",
        current.to_string(),
        vec![
            SessionConfigSelectOption::new("new", "New"),
            SessionConfigSelectOption::new("loaded", "Loaded"),
        ],
    )]
}

fn main() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let client = Rc::new(RefCell::new(None));
    let agent = MockAgent {
        client: client.clone(),
        cwd,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let (connection, io_task) = AgentSideConnection::new(
            agent,
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );
        *client.borrow_mut() = Some(Rc::new(connection));
        let _ = io_task.await;
    });
}
