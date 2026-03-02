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
