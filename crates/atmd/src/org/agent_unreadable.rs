//! Read-only screen diagnosis, never permission to type or promote a seat.
//!
//! Known limit (FIFO race probe): a rebind can commit between the final snapshot
//! and unconditional `SendMessage`. The manager may see one stale informational
//! note, but it grants no input and changes no existing delivery.

use super::agent_screen::{AgentScreen, Foreground, PaneBaseline};
use super::{
    agent_context::manager_address,
    agent_flow::ProcFacts,
    agents::{agent_screen, matches_agent, AgentRuntime},
    reporting_line::outcome_manager,
};
use crate::{
    control::reservation::{PaneCapture, PaneId, ReservationError},
    registry::PaneSession,
};
use atm_core::{
    find_harness_definition,
    harness_control::{ControlDialog, ControlScreen},
    DialogState, HarnessDefinition, InputState, ScreenFacts, SessionId, TurnState,
};
use atm_org::{Address, Agent, AgentId, AgentStatus, MessageContent, MessageKind, OrgCommand};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Semaphore},
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;

/// Geometry from an attributable read, for prose only. Never part of episode
/// identity: a resize is not a different unreadable episode by itself.
#[derive(Clone)]
pub(super) struct ShortViewport {
    pub(super) pane: PaneId,
    width: u16,
    height: u16,
}

impl std::fmt::Display for ShortViewport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pane {} was {}x{} on the last read; try making it taller so atm can verify the editor and turn state", self.pane, self.width, self.height)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ScreenSample {
    pane: PaneId,
    harness: &'static HarnessDefinition,
    proof: PaneBaseline,
    binding: Option<SessionId>,
    pub(super) unreadable: bool,
}

impl ScreenSample {
    pub(super) fn read(
        screen: &AgentScreen,
        baseline: &PaneBaseline,
        capture: &PaneCapture,
        pane: &PaneId,
        tag: Option<&str>,
        binding: Option<&SessionId>,
        control: &ControlScreen,
    ) -> Option<Self> {
        let p = &capture.inspection;
        let valid = p.pane.pane_id == pane.as_str()
            && !p.dead
            && !p.in_mode
            && p.local_agent_tag.as_deref() == tag
            && capture.format == screen.capture_format()
            && screen.foreground(capture, baseline) == Some(Foreground::Harness);
        valid.then_some(())?;
        Some(Self {
            pane: pane.clone(),
            harness: find_harness_definition(screen.harness().short_tag())?,
            proof: PaneBaseline::adopted(p.pane_root_pid()?)
                .with_harness(p.foreground_command.clone()?)?,
            binding: binding.cloned(),
            unreadable: control.dialog == ControlDialog::Unknown
                && control.facts == ScreenFacts::default(),
        })
    }

    /// Call with the same accepted capture and verdict as `read`.
    pub(super) fn short_viewport(
        &self,
        capture: &PaneCapture,
        control: &ControlScreen,
    ) -> Option<ShortViewport> {
        let (width, height) = (
            capture.inspection.pane.width,
            capture.inspection.pane.height,
        );
        let facts = &control.facts;
        let incomplete = facts.input == InputState::Unknown
            || facts.turn == TurnState::Unknown
            || facts.dialog == DialogState::Unknown
            || control.dialog == ControlDialog::Unknown;
        (width > 0
            && (1..=12).contains(&height)
            && incomplete
            && facts.input != InputState::Draft
            && facts.turn != TurnState::Running
            && matches!(facts.dialog, DialogState::None | DialogState::Unknown)
            && matches!(control.dialog, ControlDialog::None | ControlDialog::Unknown))
        .then(|| ShortViewport {
            pane: self.pane.clone(),
            width,
            height,
        })
    }

    fn belongs_to(&self, agent: &Agent) -> bool {
        agent.pane.as_deref() == Some(self.pane.as_str())
            && agent.session_id == self.binding
            && matches!(agent.status, AgentStatus::Starting | AgentStatus::Ready)
            && agent
                .role
                .as_ref()
                .is_none_or(|r| r.config.harness == self.harness.harness)
    }
}

pub(super) type ForegroundSession = (u32, SessionId);

/// The registered root process must lead the pane's foreground group.
/// Merely being registered on that pane also matches a background job.
pub(super) fn foreground_session<'a>(
    rows: impl Iterator<Item = &'a PaneSession>,
    facts: &ProcFacts,
    binding: Option<&SessionId>,
) -> Option<ForegroundSession> {
    let pid = u32::try_from(facts.foreground_group).ok()?;
    let mut candidates =
        rows.filter(|r| r.pid == Some(pid) && binding.is_none_or(|id| id == &r.view.id));
    let row = candidates.next()?;
    (pid > 0 && candidates.next().is_none()).then(|| (pid, row.view.id.clone()))
}

const MAX_CONFIRMATION_FAILURES: u8 = 12;
/// How often the revalidation poll looks again at the seat of a dormant
/// episode: its screen can become readable with no harness event (s3-5c S1).
const RELOOK_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq)]
struct Episode {
    sample: ScreenSample,
    since: Instant,
    // None means the append was attempted or the confirmation budget is spent.
    due: Option<Instant>,
    cold: Option<ForegroundSession>,
    reported: bool,
    failures: u8,
    // Once dormant (`due` None): when the revalidation poll may look again.
    relook: Option<Instant>,
}

#[derive(Default)]
struct Entry {
    seen: Option<Instant>,
    fence: u64,
    episode: Option<Episode>,
}

pub(super) struct ScreenEpisodes {
    entries: watch::Sender<BTreeMap<AgentId, Entry>>,
    after: Duration,
}

impl ScreenEpisodes {
    pub(super) fn new(after: Duration) -> Self {
        Self {
            entries: watch::channel(BTreeMap::new()).0,
            after,
        }
    }

    pub(super) fn record(
        &self,
        id: &AgentId,
        sample: ScreenSample,
        at: Instant,
        cursor: u64,
        cold: Option<ForegroundSession>,
    ) -> Option<Instant> {
        let mut since = None;
        self.entries.send_if_modified(|entries| {
            let entry = entries.entry(id.clone()).or_default();
            if cursor < entry.fence || entry.seen.is_some_and(|seen| at < seen) {
                return false;
            }
            entry.seen = Some(at);
            if !sample.unreadable {
                entry.fence = cursor;
                return entry.episode.take().is_some();
            }
            if let Some(old) = entry.episode.as_ref().filter(|ep| {
                ep.sample == sample
                    && ep
                        .cold
                        .as_ref()
                        .zip(cold.as_ref())
                        .is_none_or(|(a, b)| a == b)
            }) {
                since = Some(old.since);
                return false;
            }
            entry.fence = cursor;
            entry.episode = at.checked_add(self.after).map(|due| Episode {
                sample,
                since: at,
                due: Some(due),
                cold,
                reported: false,
                failures: 0,
                relook: None,
            });
            since = entry.episode.as_ref().map(|ep| ep.since);
            true
        });
        since
    }

    pub(super) fn invalidate(&self, id: &AgentId, event_seq: u64) {
        self.entries.send_if_modified(|entries| {
            let entry = entries.entry(id.clone()).or_default();
            if event_seq <= entry.fence {
                return false;
            }
            entry.fence = event_seq;
            entry.episode.take().is_some()
        });
    }

    /// Whether the agent's last read opened or kept an unreadable episode:
    /// that seat is this worker's to report, not the revalidation poll's.
    pub(super) fn unreadable(&self, id: &AgentId) -> bool {
        self.entries
            .borrow()
            .get(id)
            .is_some_and(|entry| entry.episode.is_some())
    }

    /// Whether the revalidation poll may look at the seat of a dormant
    /// episode now; taking a look moves the next one a minute on. An episode
    /// has a slot only once the worker is done with it.
    pub(super) fn take_relook(&self, id: &AgentId, now: Instant) -> bool {
        let mut taken = false;
        self.entries.send_if_modified(|entries| {
            let episode = entries.get_mut(id).and_then(|entry| entry.episode.as_mut());
            if let Some(ep) = episode.filter(|ep| ep.relook.is_some_and(|at| at <= now)) {
                ep.relook = now.checked_add(RELOOK_EVERY);
                taken = true;
            }
            // Nobody waits on the next look: no wake.
            false
        });
        taken
    }

    pub(super) fn reported(&self, agent: &Agent, since: Instant) -> bool {
        self.entries
            .borrow()
            .get(&agent.id)
            .and_then(|entry| entry.episode.as_ref())
            .is_some_and(|ep| ep.since == since && ep.reported && ep.sample.belongs_to(agent))
    }

    #[cfg(test)]
    pub(super) async fn wait_for_diagnosis_to_settle(&self, id: &AgentId, since: Instant) {
        self.entries
            .subscribe()
            .wait_for(|entries| {
                entries
                    .get(id)
                    .and_then(|entry| entry.episode.as_ref())
                    .is_some_and(|ep| ep.since == since && ep.due.is_none())
            })
            .await
            .unwrap();
    }

    #[cfg(test)]
    pub(super) fn active(&self, agent: &Agent) -> bool {
        self.entries
            .borrow()
            .get(&agent.id)
            .and_then(|entry| entry.episode.as_ref())
            .is_some_and(|ep| ep.sample.belongs_to(agent))
    }

    pub(super) async fn run(rt: AgentRuntime, stop: CancellationToken) {
        let files = Arc::new(Semaphore::new(1));
        let mut changes = rt.screens.entries.subscribe();
        let work = async {
            loop {
                let next = changes
                    .borrow_and_update()
                    .iter()
                    .filter_map(|(id, entry)| {
                        let ep = entry.episode.as_ref()?;
                        Some((ep.due?, id, ep))
                    })
                    .min_by_key(|(at, ..)| *at)
                    .map(|(at, id, ep)| (at, id.clone(), ep.clone()));
                let Some((at, id, ep)) = next else {
                    if changes.changed().await.is_err() {
                        break;
                    }
                    continue;
                };
                tokio::select! { biased;
                    update = changes.changed() => { if update.is_err() { break; } continue; },
                    () = sleep_until(at) => {},
                }
                let result = confirm(&rt, &id, &ep, files.clone()).await;
                let mut exhausted = false;
                rt.screens.entries.send_if_modified(|entries| {
                    let Some(entry) = entries.get_mut(&id) else {
                        return false;
                    };
                    let Some(held) = entry.episode.as_mut().filter(|held| **held == ep) else {
                        return false;
                    };
                    match result {
                        Ok(Some(reported)) => {
                            (held.due, held.reported) = (None, reported);
                            held.relook = Instant::now().checked_add(RELOOK_EVERY);
                        }
                        Ok(None) => {
                            entry.seen = Some(Instant::now());
                            entry.episode = None;
                        }
                        Err(()) => {
                            held.failures += 1;
                            exhausted = held.failures == MAX_CONFIRMATION_FAILURES;
                            held.due =
                                (!exhausted).then(|| Instant::now() + Duration::from_secs(5));
                            held.relook = exhausted
                                .then(|| Instant::now().checked_add(RELOOK_EVERY))
                                .flatten();
                        }
                    }
                    true
                });
                if exhausted {
                    tracing::warn!(agent = %id, pane = %ep.sample.pane, failed_checks = MAX_CONFIRMATION_FAILURES,
                        "Unreadable screen confirmation retries exhausted");
                }
            }
        };
        tokio::select! { biased; () = stop.cancelled() => {}, () = work => {} }
    }
}

async fn confirm(
    rt: &AgentRuntime,
    id: &AgentId,
    ep: &Episode,
    files: Arc<Semaphore>,
) -> Result<Option<bool>, ()> {
    let sample = &ep.sample;
    let state = rt.org.snapshot().await.map_err(|_| ())?;
    let Some(agent) = state.agent(id).filter(|a| sample.belongs_to(a)) else {
        return Ok(None);
    };
    let screen = agent_screen(sample.harness.harness).ok_or(())?;
    let capture = match rt
        .control
        .observe(&sample.pane, screen.capture_format())
        .await
    {
        Ok(capture) => capture,
        Err(ReservationError::PaneGone { .. }) => return Ok(None),
        Err(_) => return Err(()),
    };
    let elapsed = ep.since.elapsed().as_secs();
    let control = screen.control(&capture, &sample.proof);
    let Some(confirmed) = ScreenSample::read(
        screen,
        &sample.proof,
        &capture,
        &sample.pane,
        Some(&id.to_string()),
        sample.binding.as_ref(),
        &control,
    )
    .filter(|confirmed| confirmed == sample) else {
        return Ok(None);
    };
    let short_viewport = confirmed.short_viewport(&capture, &control);
    let rows = rt.pane_sessions(&sample.pane).await.ok();
    let version = {
        let root = rows.iter().flatten().filter(|row| {
            matches_agent(agent, &row.view)
                && row.view.harness == sample.harness.id
                && sample
                    .binding
                    .as_ref()
                    .is_none_or(|bound| bound == &row.view.id)
        });
        if let Some(expected) = &ep.cold {
            rows.as_ref().ok_or(())?;
            let pid = capture.inspection.pane_root_pid().ok_or(())?;
            let witnessed = (rt.proc)(pid).await.and_then(|facts| {
                foreground_session(root.clone(), &facts, sample.binding.as_ref())
            });
            if witnessed.as_ref() != Some(expected) {
                return Ok(None);
            }
        }
        let mut eligible = root.filter(|row| !row.view.id.is_pending());
        eligible
            .next()
            .and_then(|row| row.harness_version.as_deref())
            .filter(|_| eligible.next().is_none())
    };
    let name = sample.harness.display_name;
    let cause = match version {
        Some(v) => format!("{name} {v} may have changed its screen"),
        None => format!("{name} may have changed its screen (version unknown)"),
    };
    let fallback = manager_address(id, agent.role.as_ref()).unwrap_or(Address::Owner);
    let to = outcome_manager(&state, id, fallback, files).await;
    if rt.org.snapshot().await.map_err(|_| ())?.agent(id) != Some(agent)
        || rt
            .screens
            .entries
            .borrow()
            .get(id)
            .and_then(|e| e.episode.as_ref())
            != Some(ep)
    {
        return Ok(None);
    }
    let diagnosis = match short_viewport {
        Some(viewport) => format!("for {elapsed} s; {viewport}"),
        None => format!("({}) for {elapsed} s; {cause}", sample.pane),
    };
    let note = MessageContent {
        delivery_notice: None, from: Address::System, to, kind: MessageKind::Note, attachments: Vec::new(), task: None,
        body: format!("atm cannot read {id}'s screen {diagnosis}. Delivery, readiness checks, clear, and handover need a readable screen."),
        post: None,
    };
    Ok(Some(rt.org.execute(OrgCommand::SendMessage(note), chrono::Utc::now()).await
        .inspect_err(|error| tracing::warn!(agent = %id, %error, "Unreadable screen note was not confirmed")).is_ok_and(|events| !events.is_empty())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_screen_late_samples_do_not_restore_or_clear_an_episode() {
        let timeout = Duration::from_secs(120);
        let tracker = ScreenEpisodes::new(timeout);
        let id = "p/worker".parse().unwrap();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let t3 = t2 + Duration::from_secs(1);
        let unknown = ScreenSample {
            pane: PaneId::parse("%7").unwrap(),
            harness: find_harness_definition("pi").unwrap(),
            proof: PaneBaseline::adopted(20).with_harness("pi".into()).unwrap(),
            binding: None,
            unreadable: true,
        };
        let readable = ScreenSample {
            unreadable: false,
            ..unknown.clone()
        };
        tracker.record(
            &id,
            unknown.clone(),
            t0,
            0,
            Some((20, SessionId::new("old"))),
        );
        tracker.record(&id, unknown.clone(), t1, 0, None);
        let due = || {
            tracker.entries.borrow()[&id]
                .episode
                .as_ref()
                .and_then(|episode| episode.due)
        };
        assert_eq!(
            due(),
            Some(t0 + timeout),
            "two producers share the first clock"
        );
        tracker.record(&id, readable.clone(), t2, 0, None);
        tracker.record(&id, unknown.clone(), t1, 0, None);
        assert_eq!(due(), None, "an older capture cannot undo recovery");
        tracker.record(&id, unknown, t3, 0, None);
        tracker.record(&id, readable, t2, 0, None);
        assert_eq!(
            due(),
            Some(t3 + timeout),
            "an older capture cannot clear the new episode"
        );
    }

    // s3-5c S1. Break: the poll re-looks while the worker still owns the
    // episode, more often than once a minute, or never once it is dormant.
    #[test]
    fn dormant_episode_gives_one_relook_a_minute() {
        let tracker = ScreenEpisodes::new(Duration::from_secs(120));
        let id: AgentId = "p/worker".parse().unwrap();
        let t0 = Instant::now();
        let sample = ScreenSample {
            pane: PaneId::parse("%7").unwrap(),
            harness: find_harness_definition("pi").unwrap(),
            proof: PaneBaseline::adopted(20).with_harness("pi".into()).unwrap(),
            binding: None,
            unreadable: true,
        };
        tracker.record(&id, sample, t0, 0, None);
        let later = |secs| t0 + Duration::from_secs(secs);
        assert!(
            !tracker.take_relook(&id, later(3600)),
            "the worker's episode"
        );
        // What the worker leaves when it is done with the episode.
        tracker.entries.send_modify(|entries| {
            let episode = entries.get_mut(&id).unwrap().episode.as_mut().unwrap();
            (episode.due, episode.relook) = (None, Some(later(180)));
        });
        assert!(!tracker.take_relook(&id, later(179)));
        assert!(tracker.take_relook(&id, later(180)));
        assert!(!tracker.take_relook(&id, later(181)), "taken: one a minute");
        assert!(!tracker.take_relook(&id, later(239)));
        assert!(tracker.take_relook(&id, later(240)));
        assert!(!tracker.take_relook(&"p/other".parse().unwrap(), later(240)));
    }
}
