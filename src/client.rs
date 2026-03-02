use agent_client_protocol::{
    self as acp, ClientSideConnection, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, Result as AcpResult, SessionNotification,
};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Notification(SessionNotification),
    Disconnected,
}

/// The real ACP Client implementation that receives data FROM the agent.
pub struct GpuiAcpClient {
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
}

#[async_trait(?Send)]
impl acp::Client for GpuiAcpClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> AcpResult<RequestPermissionResponse> {
        Ok(RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                option_id: args.options[0].option_id,
                meta: None,
            }),
            meta: None,
        })
    }

    async fn session_notification(&self, args: SessionNotification) -> AcpResult<()> {
        let _ = self.event_tx.send(AgentEvent::Notification(args));
        Ok(())
    }
}

/// The controller that holds the outgoing connection TO the agent.
pub struct AcpController {
    pub connection: ClientSideConnection,
}

impl AcpController {
    /// Creates a controller over any generic async I/O streams.
    /// This allows us to pass real process Stdio in production, or memory pipes in tests.
    pub async fn connect<R, W>(
        incoming: R,
        outgoing: W,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let client_impl = GpuiAcpClient { event_tx };

        // Convert Tokio streams to Futures streams as required by the ACP crate
        let incoming_compat = incoming.compat();
        let outgoing_compat = outgoing.compat_write();

        let (connection, handle_io) =
            ClientSideConnection::new(client_impl, outgoing_compat, incoming_compat, |fut| {
                tokio::task::spawn_local(fut);
            });

        // Run the client-side I/O handler on a LocalSet
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                handle_io.await;
            });
        });

        Ok(Self { connection })
    }
}

// =====================================================================
// TEST INFRASTRUCTURE: MOCK AGENT (Server-Side)
// =====================================================================

#[cfg(test)]
pub mod mock {
    use super::*;
    use agent_client_protocol::{
        Agent, InitializeRequest, InitializeResponse, PromptRequest, PromptResponse,
        ProtocolVersion, SessionUpdate, StopReason,
    };
    use tokio::io::duplex;

    /// A mock implementation of the Agent side of the protocol
    pub struct MockAgent;

    #[async_trait(?Send)]
    impl Agent for MockAgent {
        async fn initialize(&self, args: InitializeRequest) -> AcpResult<InitializeResponse> {
            Ok(InitializeResponse {
                protocol_version: ProtocolVersion::V1,
                agent_info: AgentInfo {
                    name: "MockAgent".into(),
                    version: "1.0.0".into(),
                },
                auth_methods: vec![],
                agent_capabilities: AgentCapabilities::default(),
                meta: None,
            })
        }

        async fn prompt(&self, args: PromptRequest) -> AcpResult<PromptResponse> {
            // For a mock, we just immediately return a successful stop.
            // In a real test, you would use an internal channel here to stream
            // `SessionUpdate`s back to the client to test the UI's reaction.
            Ok(PromptResponse {
                // Add any mock content blocks here based on the ACP schema
                stop_reason: StopReason::EndTurn,
                meta: None,
            })
        }
    }

    /// Helper to create an in-memory connected Controller + Server setup for unit tests
    pub async fn create_mock_controller() -> (AcpController, mpsc::UnboundedReceiver<AgentEvent>) {
        // Create an in-memory full-duplex pipe with a 1MB buffer
        let (client_stream, agent_stream) = duplex(1024 * 1024);

        // Split the streams into read/write halves
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (agent_read, agent_write) = tokio::io::split(agent_stream);

        // 1. Setup the Server Side (Mock Agent)
        let (agent_conn, agent_io) = ServerSideConnection::new(
            MockAgent,
            agent_write.compat_write(),
            agent_read.compat(),
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );

        // Run the agent I/O loop in the background
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                agent_io.await; // Note: Ensure the agent stays alive for the test
            });
        });

        // 2. Setup the Client Side (Our App Controller)
        let (tx, rx) = mpsc::unbounded_channel();
        let controller = AcpController::connect(client_read, client_write, tx)
            .await
            .unwrap();

        (controller, rx)
    }
}
