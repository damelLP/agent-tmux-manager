//! Work I/O on the existing session connection. Reads live here until their terminal reply.
use super::tui_decisions::DecisionController;
use atm_protocol::ResponsePayload;
use atm_tui::{
    control::ControlNotice,
    daemon::ControlHandle,
    work::{WorkEffect, WorkFailure, WorkInput},
};
use serde::de::DeserializeOwned;
use std::{
    future::{pending, Future},
    pin::Pin,
    time::Duration,
};
use tokio::time::Instant;

type Read = Pin<Box<dyn Future<Output = WorkInput> + Send>>;

pub(super) struct WorkController {
    control: ControlHandle,
    from_pane: Option<String>,
    epoch: Instant,
    windows: Option<Read>,
    org: Option<Read>,
    wake: Option<Instant>,
}
impl WorkController {
    pub(super) fn new(control: ControlHandle, from_pane: Option<String>) -> Self {
        Self {
            control,
            from_pane,
            epoch: Instant::now(),
            windows: None,
            org: None,
            wake: None,
        }
    }
    pub(super) fn now(&self) -> Duration {
        Instant::now().saturating_duration_since(self.epoch)
    }
    pub(super) fn execute(
        &mut self,
        effects: Vec<WorkEffect>,
        decisions: &mut DecisionController,
    ) -> Option<WorkInput> {
        // Every reduction replaces the timer, including a reduction with no effects.
        self.wake = None;
        let mut failure = None;
        for effect in effects {
            match effect {
                WorkEffect::RequestWindows(generation) if self.windows.is_none() => {
                    let control = self.control.clone();
                    let caller = self.from_pane.clone();
                    self.windows = Some(Box::pin(async move {
                        WorkInput::WindowsReply {
                            generation,
                            result: read(control, caller, "session.windows", 5).await,
                        }
                    }));
                }
                WorkEffect::RequestOrg(generation) if self.org.is_none() => {
                    let control = self.control.clone();
                    let caller = self.from_pane.clone();
                    self.org = Some(Box::pin(async move {
                        WorkInput::OrgReply {
                            generation,
                            result: read(control, caller, "org.tree", 12).await,
                        }
                    }));
                }
                // The reducer keeps each flight until recv returns it, even across Hidden.
                WorkEffect::RequestWindows(_) | WorkEffect::RequestOrg(_) => {}
                WorkEffect::Watch { cursor, topics } => {
                    if let Err(error) = decisions.set_work_watch(Some((cursor, topics))) {
                        failure = Some(error);
                    }
                }
                WorkEffect::Unwatch => {
                    if let Err(error) = decisions.set_work_watch(None) {
                        failure = Some(error);
                    }
                }
                WorkEffect::WakeAt(at) => self.wake = self.epoch.checked_add(at),
            }
        }
        failure.map(|error| {
            decisions.notice(&ControlNotice::WatchFailed(error));
            WorkInput::WatchFailed
        })
    }
    pub(super) async fn recv(&mut self) -> WorkInput {
        let wake = self.wake;
        tokio::select! {
            input = next(&mut self.windows) => {self.windows = None; input}
            input = next(&mut self.org) => {self.org = None; input}
            () = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => pending().await,
                }
            } => {self.wake = None; WorkInput::Wake}
        }
    }
    pub(super) fn shutdown(&mut self) {
        self.windows = None;
        self.org = None;
        self.wake = None;
    }
}
async fn next(slot: &mut Option<Read>) -> WorkInput {
    match slot {
        Some(read) => read.await,
        None => pending().await,
    }
}
async fn read<T: DeserializeOwned>(
    control: ControlHandle,
    caller: Option<String>,
    method: &str,
    seconds: u64,
) -> Result<T, WorkFailure> {
    match control
        .request(
            method.into(),
            serde_json::json!({}),
            caller,
            Duration::from_secs(seconds),
        )
        .await
    {
        Ok(ResponsePayload::Result(value)) => {
            serde_json::from_value(value).map_err(|error| WorkFailure(error.to_string()))
        }
        Ok(ResponsePayload::Error(error)) => {
            Err(WorkFailure(format!("{} ({})", error.message, error.code)))
        }
        Err(error) => Err(WorkFailure(error.to_string())),
    }
}
