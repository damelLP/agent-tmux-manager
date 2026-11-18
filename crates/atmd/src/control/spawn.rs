//! Staged spawning lets callers build commands after cwd discovery and report
//! a new pane before typing into it. No caller environment is read here.

use atm_tmux::{PaneDirection, TmuxClient};

use super::ControlError;

/// Identifies a failed Enter after a new pane's command was typed.
pub const SPAWN_ENTER_FAILURE_CONTEXT: &str = "Failed to send Enter to new pane";

/// Resolved split target, working directory and stale-caller diagnostic.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpawnContext {
    pub target_pane: String,
    pub cwd: Option<String>,
    caller_pane_missing: bool,
    // Rust lifecycle policy only; legacy RPC contexts retain their wire shape.
    #[serde(skip)]
    detached: bool,
}

impl SpawnContext {
    /// Reconstruct a prepared context supplied by a control client.
    pub fn new(target_pane: String, cwd: Option<String>, caller_pane_missing: bool) -> Self {
        Self {
            target_pane,
            cwd,
            caller_pane_missing,
            detached: false,
        }
    }

    /// Keep keyboard selection on the existing pane. Defaults to false.
    #[must_use]
    pub fn with_detached(mut self, detached: bool) -> Self {
        self.detached = detached;
        self
    }

    #[must_use]
    pub fn caller_pane_missing(&self) -> bool {
        self.caller_pane_missing
    }
}

/// Prefer the explicit target, caller pane, first active pane, then `%0`.
/// Cwd lookup failures fall back to the caller-supplied directory.
pub async fn prepare_spawn(
    client: &(impl TmuxClient + ?Sized),
    cwd: Option<&str>,
    target_pane: Option<&str>,
    caller_pane: Option<&str>,
    fallback_cwd: Option<&str>,
) -> Result<SpawnContext, ControlError> {
    let panes = client
        .list_panes()
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to list tmux panes".to_string(),
            source,
        })?;
    let current_pane = target_pane
        .or(caller_pane)
        .or_else(|| {
            panes
                .iter()
                .find(|p| p.is_active)
                .map(|p| p.pane_id.as_str())
        })
        .unwrap_or("%0");
    let cwd = if let Some(dir) = cwd {
        Some(dir.to_string())
    } else {
        client
            .get_pane_cwd(current_pane)
            .await
            .ok()
            .flatten()
            .or_else(|| fallback_cwd.map(str::to_string))
    };
    Ok(SpawnContext {
        target_pane: current_pane.to_string(),
        cwd,
        caller_pane_missing: target_pane.is_none()
            && caller_pane.is_some()
            && !panes.iter().any(|p| p.pane_id == current_pane),
        detached: false,
    })
}

/// Prepare the TUI's largest non-caller target across the caller's session.
/// A missing or stale caller leaves the search unrestricted; equal areas keep
/// the last pane in list order. List/cwd failures are ignored, with the caller
/// then `%0` as target fallbacks. The caller still controls split cwd and typing.
/// This deliberately differs from [`prepare_spawn`]'s target/error policy.
pub async fn prepare_spawn_largest(
    client: &(impl TmuxClient + ?Sized),
    caller_pane: Option<&str>,
) -> SpawnContext {
    let panes = client.list_panes().await.ok().unwrap_or_default();
    let current_session = caller_pane
        .and_then(|ap| panes.iter().find(|p| p.pane_id == ap))
        .map(|p| p.session_name.as_str());
    let target = panes
        .iter()
        .filter(|p| {
            current_session.is_none_or(|s| p.session_name == s)
                && caller_pane != Some(p.pane_id.as_str())
        })
        .max_by_key(|p| (p.width as u32) * (p.height as u32))
        .map(|p| p.pane_id.as_str())
        .or(caller_pane)
        .unwrap_or("%0");
    let cwd = client.get_pane_cwd(target).await.ok().flatten();
    SpawnContext::new(target.to_string(), cwd, false)
}

/// Split without a command so the pane gets an interactive shell, with the
/// user's PATH and aliases. Return its id before any command is typed.
pub async fn split_spawn(
    client: &(impl TmuxClient + ?Sized),
    context: &SpawnContext,
    direction: PaneDirection,
    size: &str,
) -> Result<String, ControlError> {
    client
        .split_window(
            &context.target_pane,
            size,
            direction,
            None,
            context.cwd.as_deref(),
            context.detached,
        )
        .await
        .map_err(|source| ControlError::Tmux {
            // A stale caller deserves a hint, but a small pane does not.
            context: if context.caller_pane_missing {
                format!(
                    "Failed to split $TMUX_PANE ({}); pass -t <pane> to split another pane",
                    context.target_pane
                )
            } else {
                "Failed to split tmux pane".to_string()
            },
            source,
        })
}

/// Type an already-built command, then Enter in a separate tmux call.
pub async fn type_spawn_command(
    client: &(impl TmuxClient + ?Sized),
    pane: &str,
    command: &str,
) -> Result<(), ControlError> {
    client
        .send_keys(pane, command)
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to send command to new pane".to_string(),
            source,
        })?;
    client
        .send_keys(pane, "Enter")
        .await
        .map_err(|source| ControlError::Tmux {
            context: SPAWN_ENTER_FAILURE_CONTEXT.to_string(),
            source,
        })
}

#[cfg(test)]
mod largest_tests {
    use super::*;
    use atm_tmux::{mock::MockCall, MockTmuxClient, PaneInfo, TmuxError};
    use std::{future::Future, pin::Pin};

    fn pane(id: &str, session: &str, window: u32, width: u16, height: u16) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: window,
            pane_pid: 1,
            width,
            height,
            is_active: true,
        }
    }

    async fn assert_prepared(
        client: &MockTmuxClient,
        caller: Option<&str>,
        target: &str,
        cwd: Option<&str>,
    ) {
        let context = prepare_spawn_largest(client, caller).await;
        assert_eq!(context.target_pane, target);
        assert_eq!(context.cwd.as_deref(), cwd);
        assert_eq!(
            client.calls(),
            vec![
                MockCall::ListPanes,
                MockCall::GetPaneCwd {
                    pane: target.into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn largest_uses_all_caller_session_windows_and_excludes_only_the_caller() {
        let client = MockTmuxClient::new();
        client.set_panes(vec![
            pane("%1", "work", 0, 400, 100),
            pane("%2", "work", 0, 80, 50),
            pane("%3", "work", 1, 100, 60),
            pane("%4", "work-extra", 0, 600, 300),
            pane("%5", "Work", 0, 600, 300),
        ]);
        client.set_pane_cwd("%3", "/target");
        assert_prepared(&client, Some("%1"), "%3", Some("/target")).await;
    }

    #[tokio::test]
    async fn largest_keeps_the_last_equal_area_pane_in_list_order() {
        for (first, last) in [("%9", "%2"), ("%2", "%9")] {
            let client = MockTmuxClient::new();
            client.set_panes(vec![
                pane("%1", "work", 0, 20, 20),
                pane(first, "work", 0, 80, 30),
                pane(last, "work", 1, 60, 40),
            ]);
            assert_prepared(&client, Some("%1"), last, None).await;
        }
    }

    #[tokio::test]
    async fn largest_compares_area_without_u16_overflow() {
        let client = MockTmuxClient::new();
        client.set_panes(vec![
            pane("%1", "work", 0, 20, 20),
            pane("%2", "work", 0, 40000, 2),
            pane("%3", "work", 0, 200, 200),
        ]);
        assert_prepared(&client, Some("%1"), "%2", None).await;
    }

    #[tokio::test]
    async fn missing_or_stale_caller_selects_the_largest_across_all_sessions() {
        for caller in [None, Some("%999")] {
            let client = MockTmuxClient::new();
            client.set_panes(vec![
                pane("%1", "work", 0, 20, 20),
                pane("%2", "other", 0, 100, 60),
            ]);
            client.set_pane_cwd("%2", "/other");
            assert_prepared(&client, caller, "%2", Some("/other")).await;
        }
    }

    #[tokio::test]
    async fn no_candidate_falls_back_to_caller_then_percent_zero() {
        for (caller, panes, target) in [
            (
                Some("%1"),
                vec![
                    pane("%1", "work", 0, 20, 20),
                    pane("%2", "other", 0, 100, 60),
                ],
                "%1",
            ),
            (Some("%999"), vec![], "%999"),
            (None, vec![], "%0"),
        ] {
            let client = MockTmuxClient::new();
            client.set_panes(panes);
            assert_prepared(&client, caller, target, None).await;
        }
    }

    #[tokio::test]
    async fn list_failure_is_ignored_and_fallback_cwd_is_still_queried() {
        for (caller, target) in [(Some("%7"), "%7"), (None, "%0")] {
            let client = MockTmuxClient::new();
            client.set_next_error(TmuxError::NotFound);
            client.set_pane_cwd(target, "/fallback-pane");
            assert_prepared(&client, caller, target, Some("/fallback-pane")).await;
        }
    }

    #[tokio::test]
    async fn cwd_failure_is_ignored_without_losing_the_selected_target() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![pane("%7", "work", 0, 100, 60)]);
        mock.set_pane_cwd("%7", "/unavailable");
        let client = CwdFailure(mock.clone());
        let context = prepare_spawn_largest(&client, None).await;
        assert_eq!(context.target_pane, "%7");
        assert_eq!(context.cwd, None);
        assert_eq!(
            mock.calls(),
            vec![
                MockCall::ListPanes,
                MockCall::GetPaneCwd { pane: "%7".into() }
            ]
        );
    }

    #[tokio::test]
    async fn largest_preparation_precedes_direct_split_command_and_separate_enter() {
        for direction in [
            PaneDirection::Left,
            PaneDirection::Right,
            PaneDirection::Above,
            PaneDirection::Below,
        ] {
            let mock = MockTmuxClient::new();
            mock.set_panes(vec![
                pane("%1", "work", 0, 20, 60),
                pane("%7", "work", 0, 100, 60),
            ]);
            mock.set_pane_cwd("%7", "/target");
            mock.set_next_pane_id("%42");
            let client: &dyn TmuxClient = &mock;
            let context = prepare_spawn_largest(client, Some("%1")).await;
            assert_eq!(context.target_pane, "%7");
            assert_eq!(context.cwd.as_deref(), Some("/target"));
            // As in the TUI: cwd builds the command, never split-window's -c.
            let command = "cd '/target' && inert-agent";
            let new_pane = client
                .split_window(&context.target_pane, "50%", direction, None, None, false)
                .await
                .unwrap();
            client.send_keys(&new_pane, command).await.unwrap();
            client.send_keys(&new_pane, "Enter").await.unwrap();
            assert_eq!(new_pane, "%42");
            assert_eq!(
                mock.calls(),
                vec![
                    MockCall::ListPanes,
                    MockCall::GetPaneCwd { pane: "%7".into() },
                    MockCall::SplitWindow {
                        target: "%7".into(),
                        size: "50%".into(),
                        direction,
                        command: None,
                        cwd: None,
                        detached: false,
                    },
                    MockCall::SendKeys {
                        pane: "%42".into(),
                        keys: command.into()
                    },
                    MockCall::SendKeys {
                        pane: "%42".into(),
                        keys: "Enter".into()
                    },
                ]
            );
        }
    }

    // The mock's one-shot error otherwise hits list_panes first. Delegate every
    // method to that same recorder, injecting the error only at the cwd read.
    struct CwdFailure(MockTmuxClient);
    type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
    macro_rules! forward {
        ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
            fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
            where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
                Box::pin(self.0.$method($($arg),*))
            }
        };
    }
    impl TmuxClient for CwdFailure {
        fn get_pane_cwd<'s, 'p, 'async_trait>(
            &'s self,
            pane: &'p str,
        ) -> Call<'async_trait, Option<String>>
        where
            's: 'async_trait,
            'p: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.0.set_next_error(TmuxError::NotFound);
                self.0.get_pane_cwd(pane).await
            })
        }
        forward!(raw_command<'a, 'b, 'c>(command: &'a str, args: &'b [&'c str]) -> String);
        forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
        forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
        forward!(kill_pane<'a>(pane: &'a str) -> ());
        forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
        forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
        forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
        forward!(list_panes<>() -> Vec<PaneInfo>);
        forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
        forward!(select_pane<'a>(pane: &'a str) -> ());
        forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
        forward!(new_session<'a>(name: &'a str) -> String);
    }
}
