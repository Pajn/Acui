# AGENTS.md

## Engineering rules

- Prefer established ecosystem crates over custom implementations when a stable crate exists.
- Keep code changes small, focused, and covered by tests at the seam being changed.

## Testing strategy

This project tests boundaries and known friction points rather than trivial internals.

1. **Network/State boundary**
   - Verify incoming ACP JSON-RPC chunks are folded into `AppState` and domain models correctly.
   - Use protocol-native mocks and realistic streaming behavior.

2. **Async/Sync boundary**
   - Verify `tokio` background tasks that call back into GPUI context update state safely.
   - Cover task completion/error paths and connection lifecycle transitions.

3. **State/UI boundary**
   - Verify state changes drive the expected UI behavior (for example: scrolling/indicators/thread state).

## Regression-first workflow for difficult issues

If you hit a non-trivial borrow/lifetime/concurrency/logic issue:

- Stop speculative implementation changes.
- Add a focused reproducer test first.
- Fix against that test.
- Keep the test as permanent regression coverage.

## Mocking policy

- Do not hide ACP behind custom testing-only abstractions.
- Prefer protocol-native tests with `agent-client-protocol` behavior and `tokio::io::duplex` style transport simulation.
