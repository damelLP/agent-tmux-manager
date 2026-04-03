//! Mock tmux client for testing.
//!
//! Records all method calls and returns configurable results.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{PaneInfo, TmuxClient, TmuxError};

/// A recorded call to the mock tmux client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockCall {
    SplitWindow {
        target: String,
        size: String,
        horizontal: bool,
        command: Option<String>,
    },
    NewWindow {
        session: String,
        command: Option<String>,
    },
    KillPane {
        pane: String,
    },
    ResizePane {
        pane: String,
        width: Option<u16>,
        height: Option<u16>,
    },
    SendKeys {
        pane: String,
        keys: String,
    },
    ListPanes,
    DisplayPopup {
        width: String,
        height: String,
        command: String,
    },
    SelectPane {
        pane: String,
    },
}

/// Mock tmux client that records calls for test verification.
///
/// Thread-safe via `Arc<Mutex<_>>` interior mutability so it satisfies
/// `Send + Sync` required by `TmuxClient`.
///
/// # Example
///
/// ```no_run
/// use atm_tmux::{MockTmuxClient, TmuxClient};
/// use atm_tmux::mock::MockCall;
///
/// # async fn example() {
/// let mock = MockTmuxClient::new();
/// mock.set_next_pane_id("%10");
///
/// let pane_id = mock.split_window("%5", "30%", true, None).await.unwrap();
/// assert_eq!(pane_id, "%10");
///
/// let calls = mock.calls();
/// assert_eq!(calls.len(), 1);
/// assert!(matches!(&calls[0], MockCall::SplitWindow { target, .. } if target == "%5"));
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct MockTmuxClient {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug)]
struct MockState {
    calls: Vec<MockCall>,
    /// Queue of pane IDs returned by split_window/new_window.
    /// Pops from front. Falls back to "%99" when empty.
    pane_id_queue: Vec<String>,
    /// Panes returned by list_panes.
    panes: Vec<PaneInfo>,
    /// If set, the next call will return this error.
    next_error: Option<TmuxError>,
}

impl MockTmuxClient {
    /// Creates a new mock with empty state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState {
                calls: Vec::new(),
                pane_id_queue: Vec::new(),
                panes: Vec::new(),
                next_error: None,
            })),
        }
    }

    /// Queues a pane ID to be returned by the next `split_window` or `new_window` call.
    pub fn set_next_pane_id(&self, pane_id: &str) {
        if let Ok(mut state) = self.inner.lock() {
            state.pane_id_queue.push(pane_id.to_string());
        }
    }

    /// Sets the panes returned by `list_panes`.
    pub fn set_panes(&self, panes: Vec<PaneInfo>) {
        if let Ok(mut state) = self.inner.lock() {
            state.panes = panes;
        }
    }

    /// Makes the next call return an error.
    pub fn set_next_error(&self, error: TmuxError) {
        if let Ok(mut state) = self.inner.lock() {
            state.next_error = Some(error);
        }
    }

    /// Returns all recorded calls.
    pub fn calls(&self) -> Vec<MockCall> {
        self.inner
            .lock()
            .map(|state| state.calls.clone())
            .unwrap_or_default()
    }

    /// Returns the number of recorded calls.
    pub fn call_count(&self) -> usize {
        self.inner
            .lock()
            .map(|state| state.calls.len())
            .unwrap_or(0)
    }

    /// Clears all recorded calls.
    pub fn clear_calls(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.calls.clear();
        }
    }

    /// Records a call and checks for a queued error.
    fn record(&self, call: MockCall) -> Result<(), TmuxError> {
        if let Ok(mut state) = self.inner.lock() {
            state.calls.push(call);
            if let Some(err) = state.next_error.take() {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Pops the next pane ID from the queue, defaulting to "%99".
    fn next_pane_id(&self) -> String {
        self.inner
            .lock()
            .ok()
            .and_then(|mut state| {
                if state.pane_id_queue.is_empty() {
                    None
                } else {
                    Some(state.pane_id_queue.remove(0))
                }
            })
            .unwrap_or_else(|| "%99".to_string())
    }
}

impl Default for MockTmuxClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TmuxClient for MockTmuxClient {
    async fn split_window(
        &self,
        target: &str,
        size: &str,
        horizontal: bool,
        command: Option<&str>,
    ) -> Result<String, TmuxError> {
        let pane_id = self.next_pane_id();
        self.record(MockCall::SplitWindow {
            target: target.to_string(),
            size: size.to_string(),
            horizontal,
            command: command.map(|s| s.to_string()),
        })?;
        Ok(pane_id)
    }

    async fn new_window(
        &self,
        session: &str,
        command: Option<&str>,
    ) -> Result<String, TmuxError> {
        let pane_id = self.next_pane_id();
        self.record(MockCall::NewWindow {
            session: session.to_string(),
            command: command.map(|s| s.to_string()),
        })?;
        Ok(pane_id)
    }

    async fn kill_pane(&self, pane: &str) -> Result<(), TmuxError> {
        self.record(MockCall::KillPane {
            pane: pane.to_string(),
        })
    }

    async fn resize_pane(
        &self,
        pane: &str,
        width: Option<u16>,
        height: Option<u16>,
    ) -> Result<(), TmuxError> {
        self.record(MockCall::ResizePane {
            pane: pane.to_string(),
            width,
            height,
        })
    }

    async fn send_keys(&self, pane: &str, keys: &str) -> Result<(), TmuxError> {
        self.record(MockCall::SendKeys {
            pane: pane.to_string(),
            keys: keys.to_string(),
        })
    }

    async fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        self.record(MockCall::ListPanes)?;
        let panes = self
            .inner
            .lock()
            .map(|state| state.panes.clone())
            .unwrap_or_default();
        Ok(panes)
    }

    async fn display_popup(
        &self,
        width: &str,
        height: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.record(MockCall::DisplayPopup {
            width: width.to_string(),
            height: height.to_string(),
            command: command.to_string(),
        })
    }

    async fn select_pane(&self, pane: &str) -> Result<(), TmuxError> {
        self.record(MockCall::SelectPane {
            pane: pane.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_records_calls() {
        let mock = MockTmuxClient::new();
        mock.set_next_pane_id("%10");

        let pane_id = mock
            .split_window("%5", "30%", true, Some("claude"))
            .await
            .unwrap();
        assert_eq!(pane_id, "%10");

        mock.kill_pane("%10").await.unwrap();
        mock.send_keys("%5", "hello").await.unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(
            &calls[0],
            MockCall::SplitWindow {
                target,
                horizontal: true,
                ..
            } if target == "%5"
        ));
        assert!(matches!(&calls[1], MockCall::KillPane { pane } if pane == "%10"));
        assert!(matches!(&calls[2], MockCall::SendKeys { keys, .. } if keys == "hello"));
    }

    #[tokio::test]
    async fn test_mock_default_pane_id() {
        let mock = MockTmuxClient::new();
        let pane_id = mock.split_window("%1", "50%", false, None).await.unwrap();
        assert_eq!(pane_id, "%99");
    }

    #[tokio::test]
    async fn test_mock_pane_id_queue() {
        let mock = MockTmuxClient::new();
        mock.set_next_pane_id("%10");
        mock.set_next_pane_id("%11");

        let p1 = mock.split_window("%1", "50%", false, None).await.unwrap();
        let p2 = mock.new_window("sess", None).await.unwrap();
        let p3 = mock.split_window("%1", "50%", false, None).await.unwrap();

        assert_eq!(p1, "%10");
        assert_eq!(p2, "%11");
        assert_eq!(p3, "%99"); // exhausted queue
    }

    #[tokio::test]
    async fn test_mock_error_injection() {
        let mock = MockTmuxClient::new();
        mock.set_next_error(TmuxError::PaneNotFound("%999".to_string()));

        let result = mock.kill_pane("%999").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TmuxError::PaneNotFound(p) if p == "%999"));

        // Next call should succeed (error was consumed)
        let result = mock.kill_pane("%5").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mock_list_panes() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![
            PaneInfo {
                pane_id: "%1".to_string(),
                session_name: "main".to_string(),
                window_index: 0,
                pane_pid: 1234,
                width: 80,
                height: 24,
                is_active: true,
            },
            PaneInfo {
                pane_id: "%2".to_string(),
                session_name: "main".to_string(),
                window_index: 0,
                pane_pid: 5678,
                width: 80,
                height: 24,
                is_active: false,
            },
        ]);

        let panes = mock.list_panes().await.unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert!(panes[0].is_active);
    }

    #[tokio::test]
    async fn test_mock_clear_calls() {
        let mock = MockTmuxClient::new();
        mock.select_pane("%1").await.unwrap();
        assert_eq!(mock.call_count(), 1);

        mock.clear_calls();
        assert_eq!(mock.call_count(), 0);
    }

    #[tokio::test]
    async fn test_mock_display_popup() {
        let mock = MockTmuxClient::new();
        mock.display_popup("80%", "60%", "atm").await.unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            MockCall::DisplayPopup { command, .. } if command == "atm"
        ));
    }

    #[tokio::test]
    async fn test_mock_resize_pane() {
        let mock = MockTmuxClient::new();
        mock.resize_pane("%5", Some(120), None).await.unwrap();

        let calls = mock.calls();
        assert!(matches!(
            &calls[0],
            MockCall::ResizePane {
                pane,
                width: Some(120),
                height: None,
            } if pane == "%5"
        ));
    }
}
