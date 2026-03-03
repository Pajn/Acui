use agent_client_protocol::{
    self as acp, Agent, AgentCapabilities, AgentSideConnection, AuthenticateRequest,
    AuthenticateResponse, AvailableCommand, AvailableCommandsUpdate, CancelNotification, Client,
    ContentBlock, ContentChunk, Cost, CurrentModeUpdate, ForkSessionRequest, ForkSessionResponse,
    Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, ModelId, ModelInfo,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, PromptRequest, PromptResponse, ReadTextFileRequest,
    RequestPermissionOutcome, RequestPermissionRequest, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionConfigOption, SessionConfigSelectOption,
    SessionForkCapabilities, SessionInfo, SessionListCapabilities, SessionMode, SessionModeId,
    SessionModeState, SessionModelState, SessionNotification, SessionResumeCapabilities,
    SessionUpdate, SetSessionModeRequest, SetSessionModeResponse, SetSessionModelRequest,
    SetSessionModelResponse, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UsageUpdate, WaitForTerminalExitRequest, WriteTextFileRequest,
};
use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

struct MockAgent {
    client: Rc<RefCell<Option<Rc<AgentSideConnection>>>>,
    cwd: PathBuf,
    session_modes: Rc<RefCell<HashMap<String, String>>>,
    session_models: Rc<RefCell<HashMap<String, String>>>,
    session_cwds: Rc<RefCell<HashMap<String, PathBuf>>>,
}

impl MockAgent {
    fn client(&self) -> acp::Result<Rc<AgentSideConnection>> {
        self.client
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(acp::Error::internal_error)
    }

    fn maybe_seed_session(&self) {
        if std::env::var("ACUI_MOCK_SEED_SESSION").as_deref() != Ok("1") {
            return;
        }
        let session_id = std::env::var("ACUI_MOCK_SEED_SESSION_ID")
            .unwrap_or_else(|_| "mock-seeded-session".to_string());
        let cwd = std::env::var("ACUI_MOCK_SEED_CWD")
            .map(PathBuf::from)
            .unwrap_or_else(|_| self.cwd.clone());
        if self.session_cwds.borrow().contains_key(&session_id) {
            return;
        }
        self.session_modes
            .borrow_mut()
            .insert(session_id.clone(), "ask".to_string());
        self.session_models
            .borrow_mut()
            .insert(session_id.clone(), "gpt-5".to_string());
        self.session_cwds.borrow_mut().insert(session_id, cwd);
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
            AvailableCommand::new("plan", "Emit a sample execution plan"),
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
        self.maybe_seed_session();
        let load_session = std::env::var("ACUI_MOCK_DISABLE_LOAD").as_deref() != Ok("1");
        Ok(InitializeResponse::new(args.protocol_version)
            .agent_info(Implementation::new("acui-mock-agent", "0.1.0"))
            .agent_capabilities(
                AgentCapabilities::new()
                    .load_session(load_session)
                    .session_capabilities(
                        SessionCapabilities::new()
                            .list(SessionListCapabilities::new())
                            .fork(SessionForkCapabilities::new())
                            .resume(SessionResumeCapabilities::new()),
                    ),
            ))
    }

    async fn authenticate(&self, _args: AuthenticateRequest) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, args: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        let session_id = acp::SessionId::new(format!("mock-session-{}", uuid::Uuid::new_v4()));
        self.session_modes
            .borrow_mut()
            .insert(session_id.to_string(), "ask".to_string());
        self.session_models
            .borrow_mut()
            .insert(session_id.to_string(), "gpt-5".to_string());
        self.session_cwds
            .borrow_mut()
            .insert(session_id.to_string(), args.cwd.clone());
        self.send_available_commands(&session_id).await?;
        Ok(NewSessionResponse::new(session_id.clone())
            .modes(mode_state("ask"))
            .models(model_state("gpt-5"))
            .config_options(session_config_options("new")))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> acp::Result<LoadSessionResponse> {
        if std::env::var("ACUI_MOCK_DISABLE_LOAD").as_deref() == Ok("1") {
            return Err(acp::Error::method_not_found());
        }
        let current_mode = self
            .session_modes
            .borrow()
            .get(&args.session_id.to_string())
            .cloned()
            .unwrap_or_else(|| "ask".to_string());
        let current_model = self
            .session_models
            .borrow()
            .get(&args.session_id.to_string())
            .cloned()
            .unwrap_or_else(|| "gpt-5".to_string());
        self.session_cwds
            .borrow_mut()
            .insert(args.session_id.to_string(), args.cwd.clone());
        self.send_available_commands(&args.session_id).await?;
        Ok(LoadSessionResponse::new()
            .modes(mode_state(current_mode))
            .models(model_state(current_model))
            .config_options(session_config_options("loaded")))
    }

    async fn list_sessions(&self, args: ListSessionsRequest) -> acp::Result<ListSessionsResponse> {
        let cwd_filter = args.cwd;
        let sessions = self
            .session_cwds
            .borrow()
            .iter()
            .filter(|(_, cwd)| match cwd_filter.as_ref() {
                Some(filter) => cwd.as_path() == filter.as_path(),
                None => true,
            })
            .map(|(session_id, cwd)| {
                SessionInfo::new(acp::SessionId::new(session_id.clone()), cwd.clone()).title(
                    format!("Session {}", session_id.chars().take(8).collect::<String>()),
                )
            })
            .collect::<Vec<_>>();
        Ok(ListSessionsResponse::new(sessions))
    }

    async fn fork_session(&self, args: ForkSessionRequest) -> acp::Result<ForkSessionResponse> {
        let new_session_id =
            acp::SessionId::new(format!("mock-session-fork-{}", uuid::Uuid::new_v4()));
        let source_session_id = args.session_id.to_string();
        let mode = self
            .session_modes
            .borrow()
            .get(&source_session_id)
            .cloned()
            .unwrap_or_else(|| "ask".to_string());
        let model = self
            .session_models
            .borrow()
            .get(&source_session_id)
            .cloned()
            .unwrap_or_else(|| "gpt-5".to_string());
        self.session_modes
            .borrow_mut()
            .insert(new_session_id.to_string(), mode.clone());
        self.session_models
            .borrow_mut()
            .insert(new_session_id.to_string(), model.clone());
        self.session_cwds
            .borrow_mut()
            .insert(new_session_id.to_string(), args.cwd.clone());
        Ok(ForkSessionResponse::new(new_session_id)
            .modes(mode_state(mode))
            .models(model_state(model))
            .config_options(session_config_options("loaded")))
    }

    async fn resume_session(
        &self,
        args: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let session_key = args.session_id.to_string();
        let current_mode = self
            .session_modes
            .borrow()
            .get(&session_key)
            .cloned()
            .unwrap_or_else(|| "ask".to_string());
        let current_model = self
            .session_models
            .borrow()
            .get(&session_key)
            .cloned()
            .unwrap_or_else(|| "gpt-5".to_string());
        self.session_cwds
            .borrow_mut()
            .insert(session_key, args.cwd.clone());
        self.send_available_commands(&args.session_id).await?;
        Ok(ResumeSessionResponse::new()
            .modes(mode_state(current_mode))
            .models(model_state(current_model))
            .config_options(session_config_options("resumed")))
    }

    async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        self.session_modes
            .borrow_mut()
            .insert(args.session_id.to_string(), args.mode_id.to_string());
        self.client()?
            .session_notification(SessionNotification::new(
                args.session_id,
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(args.mode_id)),
            ))
            .await?;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: SetSessionModelRequest,
    ) -> acp::Result<SetSessionModelResponse> {
        self.session_models
            .borrow_mut()
            .insert(args.session_id.to_string(), args.model_id.to_string());
        Ok(SetSessionModelResponse::new())
    }

    async fn prompt(&self, args: PromptRequest) -> acp::Result<PromptResponse> {
        let prompt = prompt_text(&args.prompt);
        let trimmed = prompt.trim();
        if trimmed == "cwd" {
            self.send_text(&args.session_id, format!("cwd: {}", self.cwd.display()))
                .await?;
        } else if let Some(title) = trimmed.strip_prefix("title ").map(str::trim) {
            self.client()?
                .session_notification(SessionNotification::new(
                    args.session_id.clone(),
                    SessionUpdate::SessionInfoUpdate(acp::SessionInfoUpdate::new().title(title)),
                ))
                .await?;
            self.send_text(&args.session_id, format!("title set: {title}"))
                .await?;
        } else if trimmed == "usage" {
            self.client()?
                .session_notification(SessionNotification::new(
                    args.session_id.clone(),
                    SessionUpdate::UsageUpdate(
                        UsageUpdate::new(42_000, 128_000).cost(Cost::new(1.23, "USD")),
                    ),
                ))
                .await?;
            self.send_text(&args.session_id, "usage updated").await?;
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
        } else if trimmed == "plan" {
            let plan = Plan::new(vec![
                PlanEntry::new(
                    "Inspect workspace files",
                    PlanEntryPriority::High,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "Patch implementation",
                    PlanEntryPriority::High,
                    PlanEntryStatus::InProgress,
                ),
            ]);
            self.client()?
                .session_notification(SessionNotification::new(
                    args.session_id.clone(),
                    SessionUpdate::Plan(plan),
                ))
                .await?;
            self.send_text(&args.session_id, "plan updated").await?;
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
            SessionConfigSelectOption::new("resumed", "Resumed"),
        ],
    )]
}

fn mode_state(current_mode: impl Into<SessionModeId>) -> SessionModeState {
    SessionModeState::new(
        current_mode,
        vec![
            SessionMode::new("ask", "Ask"),
            SessionMode::new("code", "Code"),
        ],
    )
}

fn model_state(current_model: impl Into<ModelId>) -> SessionModelState {
    SessionModelState::new(
        current_model,
        vec![
            ModelInfo::new("gpt-5", "GPT-5"),
            ModelInfo::new("gpt-5-mini", "GPT-5 Mini"),
        ],
    )
}

fn main() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let client = Rc::new(RefCell::new(None));
    let agent = MockAgent {
        client: client.clone(),
        cwd,
        session_modes: Rc::new(RefCell::new(HashMap::new())),
        session_models: Rc::new(RefCell::new(HashMap::new())),
        session_cwds: Rc::new(RefCell::new(HashMap::new())),
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
