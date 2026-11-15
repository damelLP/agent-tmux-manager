//! Test-only effect-return gates; no real tmux I/O or clock-driven progress.

use crate::control::reservation::testing::ScriptedTmuxClient;
use atm_tmux::{PaneDirection, PaneInfo, TmuxClient, TmuxError};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    PasteText,
    SendKeys,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Hold,
    Fail,
}

#[derive(Debug)]
enum State {
    Idle,
    Armed(Effect, Action),
    Running,
    Held(oneshot::Sender<()>),
}

/// Clones share one gate. Only typed paste/key calls are intercepted; all reads
/// and other effects retain the scripted client's behavior.
#[derive(Debug, Clone)]
pub(super) struct GatedTmux {
    inner: ScriptedTmuxClient,
    state: Arc<Mutex<State>>,
    entered: watch::Sender<bool>,
}

impl GatedTmux {
    pub(super) fn new(inner: ScriptedTmuxClient) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(State::Idle)),
            entered: watch::channel(false).0,
        }
    }

    /// Hold the next matching call's return, after its inner effect completes.
    /// Refuse a second arm while armed, running, or held, rather than silently
    /// replacing a barrier. Later matching calls pass through even during a hold.
    pub(super) fn hold_next(&self, effect: Effect) -> Result<(), &'static str> {
        self.arm(effect, Action::Hold)
    }

    /// After the next matching inner call succeeds, return a synthetic
    /// `TmuxError::Timeout`. Reservations wrap it in `ReservationError::Control`,
    /// which `paste_certainty` classifies as Unknown (input may have landed).
    /// The zero duration is error metadata only; no clock or timer is used.
    pub(super) fn fail_next_after_effect(&self, effect: Effect) -> Result<(), &'static str> {
        self.arm(effect, Action::Fail)
    }

    fn arm(&self, effect: Effect, action: Action) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, State::Idle) {
            return Err("an effect gate is already armed or held");
        }
        self.entered.send_replace(false);
        *state = State::Armed(effect, action);
        Ok(())
    }

    /// Wait for the currently armed hold's effect to finish. Entry is retained
    /// until the next arm, so subscribing after the effect cannot miss it.
    pub(super) async fn entered(&self) {
        let mut entered = self.entered.subscribe();
        let _ = entered.wait_for(|entered| *entered).await;
    }

    /// Release the current held return, including an abandoned call. No-op if
    /// not held; an early release does not release a future hold.
    pub(super) fn release(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Held(_)) {
            if let State::Held(release) = std::mem::replace(&mut *state, State::Idle) {
                let _ = release.send(());
            }
        }
    }

    async fn effect(&self, effect: Effect, call: Call<'_, ()>) -> Result<(), TmuxError> {
        let action = {
            let mut state = self.state.lock().unwrap();
            match *state {
                State::Armed(next, action) if next == effect => {
                    *state = State::Running;
                    Some(action)
                }
                _ => None,
            }
        };
        let result = call.await;
        match action {
            None => result,
            Some(Action::Fail) => {
                *self.state.lock().unwrap() = State::Idle;
                result.and(Err(TmuxError::Timeout {
                    command: format!("synthetic {effect:?} reply lost after effect"),
                    timeout: Duration::ZERO,
                }))
            }
            Some(Action::Hold) => {
                let (release, released) = oneshot::channel();
                {
                    let mut state = self.state.lock().unwrap();
                    *state = State::Held(release);
                    self.entered.send_replace(true);
                }
                let _ = released.await;
                result
            }
        }
    }
}

// Match the existing async-trait ABI without another dependency.
type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
macro_rules! forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
        where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
            Box::pin(self.inner.$method($($arg),*))
        }
    };
}

impl TmuxClient for GatedTmux {
    forward!(raw_command<'c, 'a, 'v>(subcommand: &'c str, args: &'a [&'v str]) -> String);
    forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
    forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    forward!(kill_pane<'a>(pane: &'a str) -> ());
    forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    fn send_keys<'s, 'a, 'b, 'async_trait>(
        &'s self,
        pane: &'a str,
        keys: &'b str,
    ) -> Call<'async_trait, ()>
    where
        's: 'async_trait,
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(self.effect(Effect::SendKeys, self.inner.send_keys(pane, keys)))
    }
    fn paste_text<'s, 'a, 'b, 'async_trait>(
        &'s self,
        pane: &'a str,
        text: &'b str,
    ) -> Call<'async_trait, ()>
    where
        's: 'async_trait,
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(self.effect(Effect::PasteText, self.inner.paste_text(pane, text)))
    }
    forward!(list_panes<>() -> Vec<PaneInfo>);
    forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    forward!(select_pane<'a>(pane: &'a str) -> ());
    forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    forward!(new_session<'a>(name: &'a str) -> String);
}
