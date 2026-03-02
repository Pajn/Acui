use agent_client_protocol::{
    self as acp, Agent, ClientSideConnection, ContentBlock, CreateTerminalRequest,
    CreateTerminalResponse, InitializeRequest, KillTerminalCommandRequest,
    KillTerminalCommandResponse, LoadSessionRequest, NewSessionRequest, PromptRequest,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionConfigId,
    SessionConfigOption, SessionConfigValueId, SessionId, SessionModeId, SessionModeState,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest,
    StopReason, TerminalExitStatus, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug)]
pub struct PermissionRequestEvent {
    pub options: Vec<acp::PermissionOption>,
    pub response_tx: oneshot::Sender<RequestPermissionOutcome>,
}

#[derive(Debug)]
pub enum AgentEvent {
    Notification(SessionNotification),
    PermissionRequest(PermissionRequestEvent),
    Disconnected,
}

/// The ACP client implementation that receives data FROM the agent.
pub struct GpuiAcpClient {
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
    terminals: Mutex<HashMap<String, Arc<TerminalState>>>,
}

#[derive(Debug)]
struct TerminalBuffers {
    output: String,
    truncated: bool,
    output_limit: Option<usize>,
    exit_status: Option<TerminalExitStatus>,
    did_report_exit: bool,
}

#[derive(Debug)]
struct TerminalState {
    session_id: SessionId,
    terminal_id: acp::TerminalId,
    child: Mutex<Option<tokio::process::Child>>,
    buffers: Mutex<TerminalBuffers>,
}

impl TerminalState {
    fn new(
        session_id: SessionId,
        terminal_id: acp::TerminalId,
        child: tokio::process::Child,
        output_limit: Option<u64>,
    ) -> Self {
        Self {
            session_id,
            terminal_id,
            child: Mutex::new(Some(child)),
            buffers: Mutex::new(TerminalBuffers {
                output: String::new(),
                truncated: false,
                output_limit: output_limit.map(|value| value as usize),
                exit_status: None,
                did_report_exit: false,
            }),
        }
    }
}

impl GpuiAcpClient {
    fn send_agent_message(&self, session_id: SessionId, text: impl Into<String>) {
        let update = SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(ContentBlock::from(
            text.into(),
        )));
        let _ = self
            .event_tx
            .send(AgentEvent::Notification(SessionNotification::new(
                session_id, update,
            )));
    }
}

#[async_trait(?Send)]
impl acp::Client for GpuiAcpClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> acp::Result<RequestPermissionResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        let _ = self
            .event_tx
            .send(AgentEvent::PermissionRequest(PermissionRequestEvent {
                options: args.options,
                response_tx,
            }));
        let outcome = response_rx
            .await
            .unwrap_or(RequestPermissionOutcome::Cancelled);

        Ok(RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: SessionNotification) -> acp::Result<()> {
        let _ = self.event_tx.send(AgentEvent::Notification(args));
        Ok(())
    }

    async fn read_text_file(&self, args: ReadTextFileRequest) -> acp::Result<ReadTextFileResponse> {
        let raw = std::fs::read_to_string(&args.path).map_err(map_io_error)?;
        let (content, first_line) = select_text_range(&raw, args.line, args.limit);
        self.send_agent_message(
            args.session_id,
            format_read_result(&args.path, &content, first_line),
        );
        Ok(ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: WriteTextFileRequest,
    ) -> acp::Result<WriteTextFileResponse> {
        let previous = match std::fs::read_to_string(&args.path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(map_io_error(err)),
        };
        std::fs::write(&args.path, &args.content).map_err(map_io_error)?;
        let diff = render_text_diff(previous.as_deref(), &args.content);
        self.send_agent_message(
            args.session_id,
            format!("Wrote {}.\n{}", args.path.display(), diff),
        );
        Ok(WriteTextFileResponse::new())
    }

    async fn create_terminal(
        &self,
        args: CreateTerminalRequest,
    ) -> acp::Result<CreateTerminalResponse> {
        let mut command = tokio::process::Command::new(&args.command);
        command
            .args(&args.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in &args.env {
            command.env(&var.name, &var.value);
        }
        if let Some(cwd) = &args.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(acp::Error::into_internal_error)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let terminal_id = acp::TerminalId::new(format!("terminal-{}", uuid::Uuid::new_v4()));
        let state = Arc::new(TerminalState::new(
            args.session_id.clone(),
            terminal_id.clone(),
            child,
            args.output_byte_limit,
        ));
        self.terminals
            .lock()
            .await
            .insert(terminal_id.to_string(), state.clone());

        if let Some(stdout) = stdout {
            spawn_terminal_reader(state.clone(), self.event_tx.clone(), stdout);
        }
        if let Some(stderr) = stderr {
            spawn_terminal_reader(state.clone(), self.event_tx.clone(), stderr);
        }

        let mut rendered_command = args.command;
        if !args.args.is_empty() {
            rendered_command.push(' ');
            rendered_command.push_str(&args.args.join(" "));
        }
        self.send_agent_message(
            args.session_id,
            format!(
                "$ {rendered_command}\n[terminal {}] running",
                state.terminal_id
            ),
        );

        Ok(CreateTerminalResponse::new(terminal_id))
    }

    async fn terminal_output(
        &self,
        args: TerminalOutputRequest,
    ) -> acp::Result<TerminalOutputResponse> {
        let state = self
            .terminals
            .lock()
            .await
            .get(&args.terminal_id.to_string())
            .cloned()
            .ok_or_else(|| acp::Error::resource_not_found(Some(args.terminal_id.to_string())))?;
        refresh_terminal_status(state.clone(), self.event_tx.clone()).await?;
        let buffers = state.buffers.lock().await;
        Ok(
            TerminalOutputResponse::new(buffers.output.clone(), buffers.truncated)
                .exit_status(buffers.exit_status.clone()),
        )
    }

    async fn wait_for_terminal_exit(
        &self,
        args: WaitForTerminalExitRequest,
    ) -> acp::Result<WaitForTerminalExitResponse> {
        let state = self
            .terminals
            .lock()
            .await
            .get(&args.terminal_id.to_string())
            .cloned()
            .ok_or_else(|| acp::Error::resource_not_found(Some(args.terminal_id.to_string())))?;

        refresh_terminal_status(state.clone(), self.event_tx.clone()).await?;
        if let Some(exit_status) = state.buffers.lock().await.exit_status.clone() {
            return Ok(WaitForTerminalExitResponse::new(exit_status));
        }

        let mut child =
            state.child.lock().await.take().ok_or_else(|| {
                acp::Error::resource_not_found(Some(args.terminal_id.to_string()))
            })?;
        let exit_status = terminal_exit_status_from(child.wait().await.map_err(map_io_error)?);
        report_terminal_exit(state, self.event_tx.clone(), exit_status.clone()).await;
        Ok(WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill_terminal_command(
        &self,
        args: KillTerminalCommandRequest,
    ) -> acp::Result<KillTerminalCommandResponse> {
        let state = self
            .terminals
            .lock()
            .await
            .get(&args.terminal_id.to_string())
            .cloned()
            .ok_or_else(|| acp::Error::resource_not_found(Some(args.terminal_id.to_string())))?;
        if let Some(child) = state.child.lock().await.as_mut() {
            child.kill().await.map_err(map_io_error)?;
        }
        Ok(KillTerminalCommandResponse::new())
    }

    async fn release_terminal(
        &self,
        args: ReleaseTerminalRequest,
    ) -> acp::Result<ReleaseTerminalResponse> {
        let state = self
            .terminals
            .lock()
            .await
            .remove(&args.terminal_id.to_string());
        if let Some(state) = state
            && let Some(mut child) = state.child.lock().await.take()
        {
            let _ = child.kill().await;
        }
        Ok(ReleaseTerminalResponse::new())
    }
}

fn acp_initialize_request() -> InitializeRequest {
    InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapability::new()
                .read_text_file(true)
                .write_text_file(true))
            .terminal(true),
    )
}

fn map_io_error(error: std::io::Error) -> acp::Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => acp::Error::resource_not_found(None),
        _ => acp::Error::into_internal_error(error),
    }
}

fn select_text_range(content: &str, line: Option<u32>, limit: Option<u32>) -> (String, u32) {
    let lines = content.lines().collect::<Vec<_>>();
    let start_line = line.unwrap_or(1).max(1);
    let start_index = start_line.saturating_sub(1) as usize;
    if start_index >= lines.len() {
        return (String::new(), start_line);
    }
    let end_index = limit
        .map(|value| start_index.saturating_add(value as usize))
        .unwrap_or(lines.len())
        .min(lines.len());
    (lines[start_index..end_index].join("\n"), start_line)
}

fn format_read_result(path: &Path, content: &str, start_line: u32) -> String {
    let numbered = if content.is_empty() {
        "(no content)".to_string()
    } else {
        content
            .lines()
            .enumerate()
            .map(|(offset, line)| format!("{:>4}: {line}", start_line + offset as u32))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Read {} ({} bytes)\n{}",
        path.display(),
        content.len(),
        numbered
    )
}

fn render_text_diff(old: Option<&str>, new: &str) -> String {
    let old_lines = old.map(|value| value.lines().collect::<Vec<_>>());
    let new_lines = new.lines().collect::<Vec<_>>();
    let max_len = old_lines
        .as_ref()
        .map_or(new_lines.len(), |old| old.len().max(new_lines.len()));
    let mut out = Vec::new();
    out.push("--- before".to_string());
    out.push("+++ after".to_string());
    if max_len == 0 {
        out.push("(no changes)".to_string());
        return out.join("\n");
    }
    for index in 0..max_len {
        let before = old_lines
            .as_ref()
            .and_then(|lines| lines.get(index))
            .copied();
        let after = new_lines.get(index).copied();
        if before == after {
            continue;
        }
        if let Some(before) = before {
            out.push(format!("-{before}"));
        }
        if let Some(after) = after {
            out.push(format!("+{after}"));
        }
    }
    if out.len() == 2 {
        out.push("(no changes)".to_string());
    }
    out.join("\n")
}

fn append_with_limit(output: &mut String, chunk: &str, output_limit: Option<usize>) -> bool {
    output.push_str(chunk);
    let Some(limit) = output_limit else {
        return false;
    };
    if output.len() <= limit {
        return false;
    }

    while output.len() > limit {
        if let Some((index, _)) = output.char_indices().nth(1) {
            output.drain(..index);
        } else {
            output.clear();
            break;
        }
    }
    true
}

fn spawn_terminal_reader<R>(
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    mut reader: R,
) where
    R: AsyncRead + Unpin + 'static,
{
    tokio::task::spawn_local(async move {
        let mut buf = [0_u8; 4096];
        loop {
            let read = match reader.read(&mut buf).await {
                Ok(value) => value,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            {
                let mut buffers = state.buffers.lock().await;
                let output_limit = buffers.output_limit;
                if append_with_limit(&mut buffers.output, &text, output_limit) {
                    buffers.truncated = true;
                }
            }
            let rendered = format!("[terminal {}] {text}", state.terminal_id);
            let update = SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                ContentBlock::from(rendered),
            ));
            let _ = event_tx.send(AgentEvent::Notification(SessionNotification::new(
                state.session_id.clone(),
                update,
            )));
        }
    });
}

async fn report_terminal_exit(
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    exit_status: TerminalExitStatus,
) {
    let should_report = {
        let mut buffers = state.buffers.lock().await;
        buffers.exit_status = Some(exit_status.clone());
        if buffers.did_report_exit {
            false
        } else {
            buffers.did_report_exit = true;
            true
        }
    };
    if !should_report {
        return;
    }
    let label = match (exit_status.exit_code, exit_status.signal.clone()) {
        (Some(0), _) => "finished successfully".to_string(),
        (Some(code), _) => format!("failed with exit code {code}"),
        (None, Some(signal)) => format!("terminated by signal {signal}"),
        _ => "finished".to_string(),
    };
    let message = format!("[terminal {}] {label}", state.terminal_id);
    let update =
        SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(ContentBlock::from(message)));
    let _ = event_tx.send(AgentEvent::Notification(SessionNotification::new(
        state.session_id.clone(),
        update,
    )));
}

async fn refresh_terminal_status(
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) -> acp::Result<()> {
    let status = {
        let mut child_guard = state.child.lock().await;
        if let Some(child) = child_guard.as_mut() {
            child.try_wait().map_err(acp::Error::into_internal_error)?
        } else {
            None
        }
    };
    if let Some(status) = status {
        let mut child_guard = state.child.lock().await;
        *child_guard = None;
        drop(child_guard);
        report_terminal_exit(state, event_tx, terminal_exit_status_from(status)).await;
    }
    Ok(())
}

fn terminal_exit_status_from(status: std::process::ExitStatus) -> TerminalExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        TerminalExitStatus::new()
            .exit_code(status.code().map(|value| value as u32))
            .signal(status.signal().map(|value| value.to_string()))
    }
    #[cfg(not(unix))]
    {
        TerminalExitStatus::new().exit_code(status.code().map(|value| value as u32))
    }
}

/// Controller over an ACP client-side connection.
pub struct AcpController {
    pub connection: ClientSideConnection,
}

impl AcpController {
    /// Creates a controller over generic async I/O streams.
    pub async fn connect<R, W>(
        incoming: R,
        outgoing: W,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ =
                        ready_tx.send(Err(anyhow::anyhow!("failed to build tokio runtime: {err}")));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let client_impl = GpuiAcpClient {
                    event_tx,
                    terminals: Mutex::new(HashMap::new()),
                };
                let incoming_compat = incoming.compat();
                let outgoing_compat = outgoing.compat_write();
                let (connection, handle_io) = ClientSideConnection::new(
                    client_impl,
                    outgoing_compat,
                    incoming_compat,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );

                if ready_tx.send(Ok(connection)).is_err() {
                    return;
                }
                if handle_io.await.is_err() {
                    // I/O task ended; AppState receives disconnection through protocol behavior.
                }
            });
        });

        let connection = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|err| anyhow::anyhow!("timed out waiting for ACP runtime startup: {err}"))??;
        Ok(Self { connection })
    }

    pub async fn initialize_session(
        &self,
        cwd: impl Into<PathBuf>,
    ) -> anyhow::Result<(SessionId, Vec<SessionConfigOption>, Option<SessionModeState>)> {
        let _ = self.connection.initialize(acp_initialize_request()).await?;

        let response = self
            .connection
            .new_session(NewSessionRequest::new(cwd.into()))
            .await?;

        Ok((
            response.session_id,
            response.config_options.unwrap_or_default(),
            response.modes,
        ))
    }

    pub async fn load_session(
        &self,
        session_id: SessionId,
        cwd: impl Into<PathBuf>,
    ) -> anyhow::Result<(Vec<SessionConfigOption>, Option<SessionModeState>)> {
        let _ = self.connection.initialize(acp_initialize_request()).await?;
        let response = self
            .connection
            .load_session(LoadSessionRequest::new(session_id, cwd.into()))
            .await?;
        Ok((response.config_options.unwrap_or_default(), response.modes))
    }

    pub async fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: SessionConfigId,
        value: SessionConfigValueId,
    ) -> anyhow::Result<Vec<SessionConfigOption>> {
        let response = self
            .connection
            .set_session_config_option(SetSessionConfigOptionRequest::new(
                session_id, config_id, value,
            ))
            .await?;
        Ok(response.config_options)
    }

    pub async fn send_prompt(
        &self,
        session_id: SessionId,
        content: String,
    ) -> anyhow::Result<StopReason> {
        let prompt = PromptRequest::new(session_id, vec![ContentBlock::from(content)]);
        let response = self.connection.prompt(prompt).await?;
        Ok(response.stop_reason)
    }

    pub async fn set_session_mode(
        &self,
        session_id: SessionId,
        mode_id: SessionModeId,
    ) -> anyhow::Result<()> {
        let _ = self
            .connection
            .set_session_mode(SetSessionModeRequest::new(session_id, mode_id))
            .await?;
        Ok(())
    }

    pub async fn connect_from_config(
        config_path: impl AsRef<Path>,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<(Self, Child)> {
        let config = AgentProcessConfig::from_path(config_path)?;
        let (child, stdout, stdin) = spawn_agent_process(&config)?;

        let (incoming, outgoing) = bridge_stdio(stdout, stdin);
        let controller = Self::connect(incoming, outgoing, event_tx).await?;
        Ok((controller, child))
    }
}

fn spawn_agent_process(
    config: &AgentProcessConfig,
) -> anyhow::Result<(Child, ChildStdout, ChildStdin)> {
    let mut cmd = std::process::Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if let Some(cwd) = &config.cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent process did not provide stdout"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent process did not provide stdin"))?;
    Ok((child, stdout, stdin))
}

fn bridge_stdio(
    mut child_stdout: ChildStdout,
    mut child_stdin: ChildStdin,
) -> (DuplexStream, DuplexStream) {
    let (incoming_read, mut incoming_write) = tokio::io::duplex(1024 * 1024);
    let (mut outgoing_read, outgoing_write) = tokio::io::duplex(1024 * 1024);

    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match child_stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if futures::executor::block_on(async {
                        incoming_write.write_all(&buf[..n]).await
                    })
                    .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = futures::executor::block_on(async { incoming_write.shutdown().await });
    });

    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            let read = futures::executor::block_on(async { outgoing_read.read(&mut buf).await });
            let Ok(n) = read else {
                break;
            };
            if n == 0 {
                break;
            }
            if child_stdin.write_all(&buf[..n]).is_err() {
                break;
            }
            if child_stdin.flush().is_err() {
                break;
            }
        }
    });

    (incoming_read, outgoing_write)
}

#[derive(Debug, Deserialize)]
pub struct AgentProcessConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl AgentProcessConfig {
    pub fn from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use agent_client_protocol::{
        Agent, AgentSideConnection, AuthenticateRequest, AuthenticateResponse, CancelNotification,
        Client, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
        NewSessionResponse, PromptRequest, PromptResponse, SessionUpdate, StopReason,
    };
    use tokio::io::duplex;

    pub struct MockAgent;

    #[async_trait(?Send)]
    impl Agent for MockAgent {
        async fn initialize(&self, args: InitializeRequest) -> acp::Result<InitializeResponse> {
            Ok(InitializeResponse::new(args.protocol_version)
                .agent_info(Implementation::new("MockAgent", "1.0.0")))
        }

        async fn authenticate(
            &self,
            _args: AuthenticateRequest,
        ) -> acp::Result<AuthenticateResponse> {
            Ok(AuthenticateResponse::new())
        }

        async fn new_session(&self, _args: NewSessionRequest) -> acp::Result<NewSessionResponse> {
            Ok(NewSessionResponse::new(SessionId::new("mock-session")))
        }

        async fn prompt(&self, _args: PromptRequest) -> acp::Result<PromptResponse> {
            Ok(PromptResponse::new(StopReason::EndTurn))
        }

        async fn cancel(&self, _args: CancelNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    pub async fn create_mock_controller() -> (
        AcpController,
        AgentSideConnection,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (client_stream, agent_stream) = duplex(1024 * 1024);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (agent_read, agent_write) = tokio::io::split(agent_stream);

        let (agent_conn, agent_io) = AgentSideConnection::new(
            MockAgent,
            agent_write.compat_write(),
            agent_read.compat(),
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let _ = agent_io.await;
            });
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let controller = AcpController::connect(client_read, client_write, tx)
            .await
            .expect("failed to create mock controller");

        (controller, agent_conn, rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mock_agent_notifications_flow_to_client_events() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_controller, agent_conn, mut event_rx) = create_mock_controller().await;

                let update = SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    ContentBlock::from("hello from mock"),
                ));
                let notification = SessionNotification::new(SessionId::new("mock-session"), update);

                agent_conn
                    .session_notification(notification.clone())
                    .await
                    .expect("session_notification should succeed");

                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
                        .await
                        .expect("timed out waiting for event")
                        .expect("event channel unexpectedly closed");

                match event {
                    AgentEvent::Notification(received) => {
                        assert_eq!(received.session_id, notification.session_id);
                    }
                    AgentEvent::PermissionRequest(_) => panic!("expected notification event"),
                    AgentEvent::Disconnected => panic!("expected notification event"),
                }
            })
            .await;
    }

    #[test]
    fn connect_from_config_does_not_require_callsite_localset() {
        let test_dir =
            std::env::temp_dir().join(format!("acui-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&test_dir).expect("should create temp test dir");
        let config_path = test_dir.join("agent.toml");
        std::fs::write(&config_path, "command = \"cat\"\nargs = []\n")
            .expect("should write test config");

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(AcpController::connect_from_config(&config_path, event_tx))
        }));

        assert!(
            result.is_ok(),
            "connect_from_config should not panic without a Tokio LocalSet context"
        );
        if let Ok(Ok((_controller, mut child))) = result {
            let _ = child.kill();
            let _ = child.wait();
        }

        let _ = std::fs::remove_dir_all(test_dir);
    }

    #[test]
    fn connect_does_not_require_callsite_localset() {
        let (stream_a, _stream_b) = tokio::io::duplex(1024);
        let (read, write) = tokio::io::split(stream_a);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(AcpController::connect(read, write, event_tx))
        }));

        assert!(
            result.is_ok(),
            "connect should not panic when called outside LocalSet"
        );
        assert!(matches!(result, Ok(Ok(_))), "connect should initialize");
    }

    #[test]
    fn initialize_request_advertises_fs_and_terminal_capabilities() {
        let request = acp_initialize_request();
        assert!(request.client_capabilities.fs.read_text_file);
        assert!(request.client_capabilities.fs.write_text_file);
        assert!(request.client_capabilities.terminal);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_text_file_returns_content_and_emits_numbered_preview() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let temp_dir =
                    std::env::temp_dir().join(format!("acui-read-test-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&temp_dir).expect("should create temp dir");
                let path = temp_dir.join("sample.txt");
                std::fs::write(&path, "line one\nline two\nline three\n")
                    .expect("should write sample file");

                let (_controller, agent_conn, mut event_rx) = create_mock_controller().await;
                let response = agent_conn
                    .read_text_file(
                        ReadTextFileRequest::new(SessionId::new("mock-session"), path.clone())
                            .line(2_u32)
                            .limit(2_u32),
                    )
                    .await
                    .expect("read_text_file should succeed");

                assert_eq!(response.content, "line two\nline three");
                let text = next_text_chunk(&mut event_rx).await;
                assert!(text.contains("Read"));
                assert!(text.contains("line two"));
                assert!(text.contains("line three"));
                let _ = std::fs::remove_dir_all(temp_dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_text_file_emits_diff_message() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let temp_dir =
                    std::env::temp_dir().join(format!("acui-write-test-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&temp_dir).expect("should create temp dir");
                let path = temp_dir.join("sample.txt");
                std::fs::write(&path, "old line\n").expect("should write initial file");

                let (_controller, agent_conn, mut event_rx) = create_mock_controller().await;
                agent_conn
                    .write_text_file(WriteTextFileRequest::new(
                        SessionId::new("mock-session"),
                        path.clone(),
                        "new line\n",
                    ))
                    .await
                    .expect("write_text_file should succeed");

                let text = next_text_chunk(&mut event_rx).await;
                assert!(text.contains("Wrote"));
                assert!(text.contains("-old line"));
                assert!(text.contains("+new line"));
                let _ = std::fs::remove_dir_all(temp_dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_requests_capture_output_and_exit_status() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_controller, agent_conn, mut event_rx) = create_mock_controller().await;
                let created = agent_conn
                    .create_terminal(
                        CreateTerminalRequest::new(SessionId::new("mock-session"), "sh")
                            .args(vec!["-c".to_string(), "printf terminal-ok".to_string()]),
                    )
                    .await
                    .expect("create_terminal should succeed");

                let waited = agent_conn
                    .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                        SessionId::new("mock-session"),
                        created.terminal_id.clone(),
                    ))
                    .await
                    .expect("wait_for_terminal_exit should succeed");
                assert_eq!(waited.exit_status.exit_code, Some(0));

                let output = agent_conn
                    .terminal_output(TerminalOutputRequest::new(
                        SessionId::new("mock-session"),
                        created.terminal_id.clone(),
                    ))
                    .await
                    .expect("terminal_output should succeed");
                assert!(output.output.contains("terminal-ok"));
                assert_eq!(
                    output
                        .exit_status
                        .as_ref()
                        .and_then(|status| status.exit_code),
                    Some(0)
                );

                let chunks = collect_text_chunks(&mut event_rx).await;
                let joined = chunks.join("\n");
                assert!(joined.contains("running"));
                assert!(joined.contains("terminal-ok"));
                assert!(joined.contains("finished successfully"));

                agent_conn
                    .release_terminal(ReleaseTerminalRequest::new(
                        SessionId::new("mock-session"),
                        created.terminal_id,
                    ))
                    .await
                    .expect("release_terminal should succeed");
            })
            .await;
    }

    async fn next_text_chunk(event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> String {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
                .await
                .expect("timed out waiting for event")
                .expect("event stream closed unexpectedly");
            if let AgentEvent::Notification(SessionNotification {
                update:
                    SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: ContentBlock::Text(text),
                        ..
                    }),
                ..
            }) = event
            {
                return text.text;
            }
        }
    }

    async fn collect_text_chunks(
        event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    ) -> Vec<String> {
        let mut chunks = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(150), event_rx.recv()).await
        {
            if let AgentEvent::Notification(SessionNotification {
                update:
                    SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: ContentBlock::Text(text),
                        ..
                    }),
                ..
            }) = event
            {
                chunks.push(text.text);
            }
        }
        chunks
    }
}
