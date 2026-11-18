//! Connection-to-registry ordered replay tests, using in-memory I/O only.

use super::*;
use crate::registry::replay_support::Driver;

type MemoryHandler = ConnectionHandler<
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
>;

pub(super) fn handler(driver: &Driver) -> MemoryHandler {
    let (stream, _peer) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(stream);
    ConnectionHandler::new(
        reader,
        writer,
        driver.handle.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        1,
        CancellationToken::new(),
        Arc::new(MethodRouter::default()),
    )
}

pub(super) async fn drive<T>(
    driver: &mut Driver,
    future: impl std::future::Future<Output = T>,
) -> T {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            value = &mut future => return value,
            () = driver.step_command() => {},
        }
    }
}

// Synthetic: proves the ordered boundary consumes real adapter-translated
// commands and replies, rather than a second mapping implementation.
#[tokio::test]
async fn ordered_connection_dispatch_uses_real_registry() {
    timeout(Duration::from_secs(30), async {
        let origin = std::time::Instant::now();
        let mut driver = Driver::new();
        let mut handler = handler(&driver);
        let frame = handler.handle_hook_event(serde_json::json!({
            "session_id": "ordered", "hook_event_name": "UserPromptSubmit",
            "prompt": "hello", "pid": u32::MAX, "tmux_pane": "%1"
        }));
        tokio::pin!(frame);
        loop {
            tokio::select! {
                biased;
                result = &mut frame => { result.unwrap(); break; }
                () = driver.step_command() => {}
            }
        }
        let state = driver.checkpoint(origin, chrono::Utc::now());
        assert_eq!(state["ids"]["ordered"], u32::MAX);
        assert_eq!(
            state["rows"][u32::MAX.to_string()]["domain"]["first_prompt"],
            "hello"
        );
    })
    .await
    .unwrap();
}
