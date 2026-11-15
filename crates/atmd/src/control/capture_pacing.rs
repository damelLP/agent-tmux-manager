//! Pure bounded capture scheduling. The actor executes ready commands; no sleeps
//! or cached observations here. All capture paths share the per-pane interval.
use super::Command;
use crate::control::reservation::{pane_ops::Action, PaneId, ReservationError};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

const INTERVAL: Duration = Duration::from_millis(250);
const CAPACITY: usize = 32;

#[derive(Default)]
pub(super) struct Pacing {
    after: HashMap<PaneId, Instant>,
    waiting: Vec<(Instant, PaneId, Command)>,
}

impl Pacing {
    pub(super) fn admit(&mut self, command: Command, now: Instant) -> Option<Command> {
        let (pane, deadline) = match &command {
            Command::Observe { pane, deadline, .. } => (pane, *deadline),
            Command::LegacyCapture(request) => (&request.pane, request.deadline()),
            Command::Pane {
                action:
                    Action::Capture(pane, _)
                    | Action::Input {
                        pane,
                        guard: Some(_),
                        ..
                    },
                deadline,
                ..
            } => (pane, *deadline),
            _ => return Some(command),
        };
        if now >= deadline {
            command.reject_with(ReservationError::Timeout);
            return None;
        }
        self.after.retain(|_, due| *due > now);
        let pane = pane.clone();
        if let Some(due) = self.after.get(&pane) {
            if self
                .waiting
                .iter()
                .filter(|(_, queued, _)| queued == &pane)
                .count()
                >= CAPACITY
            {
                command.reject_with(ReservationError::PacingRefused { pane: pane.clone() });
            } else {
                self.waiting
                    .push(((*due).min(deadline), pane.clone(), command));
            }
            return None;
        }
        if self.after.len() >= CAPACITY {
            command.reject_with(ReservationError::PacingRefused { pane: pane.clone() });
            return None;
        }
        self.after.insert(pane.clone(), now + INTERVAL);
        Some(command)
    }

    pub(super) fn take_ready(&mut self, now: Instant) -> Option<Command> {
        let mut ready = None;
        for (due, pane, command) in std::mem::take(&mut self.waiting) {
            if due <= now && ready.is_none() {
                ready = Some(command);
            } else {
                self.waiting.push((due, pane, command));
            }
        }
        ready
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.waiting.iter().map(|(due, _, _)| *due).min()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    pub(super) fn stop(&mut self) {
        for (due, pane, command) in std::mem::take(&mut self.waiting) {
            if matches!(command, Command::Observe { .. } | Command::LegacyCapture(_)) {
                command.reject();
            } else {
                // Admitted pane actions still fence at execution. In particular,
                // cleanup keeps its independent budget during daemon shutdown.
                self.waiting.push((due, pane, command));
            }
        }
    }
}
