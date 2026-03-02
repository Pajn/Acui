Implementation Plan: Aciu: GPUI Agent Controller

Context & Goal

You are an expert Rust developer tasked with building a high-performance desktop application using Rust and GPUI (developed by Zed). The application connects to and orchestrates multiple AI agent sessions using the official Agent-Client Protocol (ACP) crate.

The architecture must be strictly decoupled, testable, and follow a Model-View-ViewModel (MVVM) approach adapted for GPUI. Core business logic and network states must be independent of the UI layer.

Architectural Rules

    Pure Domain Models: Data structures (Workspace, Thread, Message) must be pure Rust structs devoid of UI logic.

    Protocol-Native Mocking: Network interactions use the real agent_client_protocol::Client. For testing and early phases, we mock the network by spinning up a MockAgent (implementing acp::Agent) in the background and connecting them via in-memory tokio::io::duplex pipes.

    State Management: Global application state must reside in a GPUI Model<AppState>. Background tasks (tokio) will update this model and trigger cx.notify() to reactively update the UI.

    Local UI State: Ephemeral state (text input buffers, scroll positions) resides within GPUI View<T> local states.

Implementation Phases
Phase 1: Global State Management & Protocol Abstraction

Objective: Define the pure data layer, network interface and create the GPUI Model to orchestrate app logic.

Finish implementing the actual Agent and Client traits from the agent-client-protocol crate in @src/client.rs

Finish implementing @src/state.rs. This file defines the AppState which is wrapped in a GPUI Model. It is responsible for managing the domain data and handling the asynchronous event streams coming from the AcpController.

Pay close attention to listen_to_agent_events. This is where GPUI bridges the gap between Tokio's background channels and the synchronous UI render cycle.

Implement a background task manager within AppState to listen to the GpuiAcpClient stream. When a message is received, update the relevant Thread and call cx.notify().

    Deliverable: Unit tests injecting MockAgent into AppState, simulating incoming messages, and asserting that the state updates correctly.

Phase 2: Application Layout & Sidebar

Objective: Scaffold the GPUI visual hierarchy.

Implement the UI layer using GPUI. You will create three main views: WorkspaceLayout (the root split view), SidebarView, and ChatView.

    Implement src/ui/layout.rs (Main split view), src/ui/sidebar.rs, and src/ui/chat.rs.

    Sidebar Requirements:

        A fixed-width, scrollable container.

        A global "New Workspace" button at the top.

        Render a collapsible list for each Workspace in AppState.

        Inside each workspace, render a "New Thread" button.

        Render a list of Threads under each workspace. Clicking a thread updates active_thread_id in AppState.

    Deliverable: The app compiles and opens a window showing dummy workspaces and threads populated via AppState.


Key GPUI Concepts to Use:

    Observation: Each view must use cx.observe(&app_state, |this, state, cx| cx.notify()) when initialized so it re-renders whenever the AppState changes (e.g., when a text chunk streams in).

    Layout: Use GPUI's Tailwind-like methods (.flex(), .w_full(), .h_full(), .bg(), .text_color()) to style the components.

    Smart Scrolling (Phase 4): Use a ScrollHandle or GPUI's ListState to track the scroll position of the chat. The view must programmatically scroll to the bottom when a new message arrives only if the user is already at the bottom.

Phase 3: Main Chat & Smart Scrolling

Objective: Implement the chat interface with specific scroll-locking behavior.

    Chat Input: Add a text input field at the bottom of the main view. Pressing Enter triggers AppState::send_message and clears the input.

    Message List: Render the active thread's messages in a scrollable view above the input.

    Smart Scroll Logic: * Use GPUI's ScrollState or ListState.

        Maintain a locked_to_bottom boolean in the ChatView local state.

        Rule 1: On scroll event: If current_scroll_offset + visible_height >= total_content_height, set locked_to_bottom = true. Else, false.

        Rule 2: On new message arrival (AppState change): If locked_to_bottom is true, programmatically scroll to the absolute bottom. If false, do not change the scroll position.

    Deliverable: A fully interactive UI where you can type, send messages (to the mock client), receive mock replies, and the scrollbar behaves exactly as specified.

Important Notes for testing:

    Simulating UI Events: In a real-world scenario, the user scrolling up physically changes the logical scroll offset. GPUI's ListState and ScrollHandle APIs change occasionally, so if the agent struggles to read the exact offset out of the scroll_handle in step 3, it can focus purely on asserting the boolean logic (locked_to_bottom) inside the observer block.

    cx.run_until_parked() is mandatory: Because GPUI buffers state updates to optimize frame rendering, if you don't park the executor, your assertions will run before the ChatView's observer actually fires!

Phase 4: Live Protocol Integration

Objective: Once the UI works with the mock pipes, update your instantiation of AcpController::connect.

    Read a toml config file for the command to spawn an agent.

    Use tokio::process::Command to spawn the agent subprocess.

    Take child.stdout (incoming) and child.stdin (outgoing).

    Pass these to AcpController::connect exactly as you passed the memory pipes.

---

Testing Philosophy: Test the Seams and Your Struggles
You must write automated tests to verify the integrity of this application. Do not write tests for trivial getters/setters. Instead, focus your testing efforts exclusively on Component Boundaries and Areas of Friction.

Rule 1: Mandate Boundary Testing
The most critical points of failure in this architecture are where different execution contexts meet. You must write tests for:

    The Network/State Boundary: Test that incoming JSON-RPC chunks from the tokio::io::duplex mock pipe correctly trigger this.update in the AppState and fold into the Domain models accurately.

    The Async/Sync Boundary: Test that background tokio tasks spawned via cx.spawn safely resolve and update the synchronous GPUI ModelContext without deadlocking.

    The State/UI Boundary: Use GPUI's VisualTestContext to verify that state mutations actually command the UI to update (e.g., verifying the smart-scroll boolean toggles when new messages arrive).

Rule 2: Test-Driven Resolution for "Struggles"
If you encounter a complex borrow-checker error, a lifetime issue, or a logic bug that you cannot solve in a single attempt:

    Stop writing implementation code immediately. Do not guess or hallucinate fixes.

    Write a localized, isolated test that reproduces the exact failure or edge case.

    Use the test output to incrementally solve the issue.

    Leave the test in the codebase. If you struggled with it, it is a complex edge case that requires permanent regression protection.

Rule 3: Protocol-Native Mocks Only
Do not abstract the agent-client-protocol crate behind custom traits just for testing. The protocol is the application. Always use acp::ServerSideConnection with tokio::io::duplex in your test modules to simulate real streaming scenarios.


Continue on to the next phase when the code is in a clean state, you have run clippy, fmt and all tests pass. Do a commit for each phase.
