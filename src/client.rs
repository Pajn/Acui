use agent_client_protocol::{
    self as acp, Agent, ClientSideConnection, ContentBlock, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionConfigId, SessionConfigOption, SessionConfigValueId,
    SessionId, SessionNotification, SetSessionConfigOptionRequest, StopReason,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot};
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
                let client_impl = GpuiAcpClient { event_tx };
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
    ) -> anyhow::Result<(SessionId, Vec<SessionConfigOption>)> {
        let _ = self
            .connection
            .initialize(InitializeRequest::new(acp::ProtocolVersion::V1))
            .await?;

        let response = self
            .connection
            .new_session(NewSessionRequest::new(cwd.into()))
            .await?;

        Ok((
            response.session_id,
            response.config_options.unwrap_or_default(),
        ))
    }

    pub async fn load_session(
        &self,
        session_id: SessionId,
        cwd: impl Into<PathBuf>,
    ) -> anyhow::Result<Vec<SessionConfigOption>> {
        let _ = self
            .connection
            .initialize(InitializeRequest::new(acp::ProtocolVersion::V1))
            .await?;
        let response = self
            .connection
            .load_session(LoadSessionRequest::new(session_id, cwd.into()))
            .await?;
        Ok(response.config_options.unwrap_or_default())
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
        Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
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
        use agent_client_protocol::Client;

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
}
