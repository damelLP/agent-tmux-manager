//! Local source mapping and an in-memory, last-observation cache.

use atm_core::{Harness, PlanProfileId, PlanUsageSnapshot, PlanUsageWindow, SessionId};
use atm_org::{AgentId, OrgState};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

// Only adapters that can supply a proven meter appear here. Reject typos instead
// of leaving the intended meter on its implicit profile.
const METER_SOURCES: &[(Harness, &str)] = &[(
    Harness::ClaudeCode,
    atm_claude_adapter::status_line::PLAN_METER,
)];

type SourceProfiles = HashMap<(AgentId, Harness), PlanProfileId>;
type Snapshots = BTreeMap<(PlanProfileId, &'static str), PlanUsageSnapshot>;

#[derive(Clone, Debug, Default)]
pub struct PlanUsage(Arc<Store>);

#[derive(Debug, Default)]
struct Store {
    explicit: HashMap<&'static str, SourceProfiles>,
    // Only pure cache work under this lock; never hold it across an await.
    snapshots: Mutex<Snapshots>,
}

#[derive(Clone, Copy)]
pub(crate) struct Source<'a> {
    pub session: &'a SessionId,
    pub pane: &'a str,
    pub harness: Harness,
    pub meter: &'static str,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    usage: UsageConfig,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageConfig {
    #[serde(default)]
    sources: Vec<SourceConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfig {
    agent: AgentId,
    harness: Harness,
    meter: String,
    profile: PlanProfileId,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageConfigError {
    #[error("malformed TOML or invalid [usage] settings")]
    Invalid,
    #[error("unknown [usage] source harness/meter pair")]
    UnknownSource,
    #[error("duplicate [usage] source agent/harness/meter")]
    DuplicateSource,
}

impl PlanUsage {
    pub fn parse_config(text: &str) -> Result<Self, UsageConfigError> {
        // Do not forward a TOML parse error: it can quote other config values.
        let cfg: Config = toml::from_str(text).map_err(|_| UsageConfigError::Invalid)?;
        let mut store = Store::default();
        for source in cfg.usage.sources {
            let &(_, meter) = METER_SOURCES
                .iter()
                .find(|(harness, meter)| *harness == source.harness && *meter == source.meter)
                .ok_or(UsageConfigError::UnknownSource)?;
            let previous = store
                .explicit
                .entry(meter)
                .or_default()
                .insert((source.agent, source.harness), source.profile);
            if previous.is_some() {
                return Err(UsageConfigError::DuplicateSource);
            }
        }
        Ok(Self(Arc::new(store)))
    }

    fn profile(&self, state: &OrgState, source: Source<'_>) -> Option<PlanProfileId> {
        let agent = state.agent_for_pane(source.pane)?;
        if agent.session_id.as_ref() != Some(source.session) {
            return None;
        }
        match self.0.explicit.get(source.meter) {
            Some(profiles) => profiles.get(&(agent.id.clone(), source.harness)).cloned(),
            None => PlanProfileId::new(source.meter).ok(),
        }
    }

    /// Call only after the registry has proven the event's original parent.
    pub(crate) fn observe(
        &self,
        state: &OrgState,
        source: Source<'_>,
        observed_at: DateTime<Utc>,
        mut windows: Vec<PlanUsageWindow>,
    ) {
        let Some(profile) = self.profile(state, source) else {
            return;
        };
        if windows.is_empty() || !windows.iter().all(PlanUsageWindow::is_valid) {
            return;
        }
        windows.sort_by(|a, b| (&a.scope, a.duration_minutes).cmp(&(&b.scope, b.duration_minutes)));
        let Ok(mut snapshots) = self.0.snapshots.lock() else {
            return;
        };
        let key = (profile.clone(), source.meter);
        // An identical redraw supplies no evidence that the meter was reread.
        if snapshots
            .get(&key)
            .is_some_and(|last| observed_at <= last.observed_at || last.windows == windows)
        {
            return;
        }
        snapshots.insert(
            key,
            PlanUsageSnapshot {
                profile,
                meter: source.meter.into(),
                source: source.harness,
                observed_at,
                stale: false,
                windows,
            },
        );
    }

    pub(crate) fn list(&self, now: DateTime<Utc>) -> Vec<PlanUsageSnapshot> {
        let Ok(snapshots) = self.0.snapshots.lock() else {
            return Vec::new();
        };
        snapshots
            .values()
            .cloned()
            .map(|mut sample| {
                sample.stale = now - sample.observed_at >= TimeDelta::hours(1)
                    || sample
                        .windows
                        .iter()
                        .any(|w| w.resets_at.is_some_and(|reset| now >= reset));
                sample
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_org::OrgEvent;
    use chrono::TimeDelta;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_795_000_000, 0).unwrap()
    }

    fn bindings() -> OrgState {
        let mut state = OrgState::default();
        state
            .apply(
                1,
                now(),
                &OrgEvent::ProjectRegistered {
                    project_id: "fixture".parse().unwrap(),
                    root: "/fixture".into(),
                },
            )
            .unwrap();
        for (seq, agent, pane, session) in [
            (2, "alpha", "%1", Some("a")),
            (3, "bravo", "%2", Some("b")),
            (4, "charlie", "%3", Some("c")),
            (5, "unbound", "%4", None),
        ] {
            state
                .apply(
                    seq,
                    now(),
                    &OrgEvent::AgentAdopted {
                        agent_id: format!("fixture/{agent}").parse().unwrap(),
                        pane: pane.into(),
                        session_id: session.map(SessionId::new),
                    },
                )
                .unwrap();
        }
        state
    }

    fn window(minutes: u32, percent: f64) -> PlanUsageWindow {
        PlanUsageWindow::from_epoch_seconds(minutes, percent, None).unwrap()
    }

    const EXPLICIT: &str = r#"
        [[usage.sources]]
        agent = "fixture/alpha"
        harness = "claude_code"
        meter = "claude"
        profile = "z_primary"
        [[usage.sources]]
        agent = "fixture/bravo"
        harness = "claude_code"
        meter = "claude"
        profile = "a_backup"
    "#;

    #[test]
    fn only_matching_bound_source_can_update() {
        let cache = PlanUsage::parse_config(EXPLICIT).unwrap();
        let state = bindings();
        for (pane, session, harness, meter, percent) in [
            ("%1", "a", Harness::ClaudeCode, "claude", 11.0),
            ("%2", "b", Harness::ClaudeCode, "claude", 22.0),
            // A configured meter has no fallback for unmapped seats.
            ("%3", "c", Harness::ClaudeCode, "claude", 90.0),
            ("%4", "unbound", Harness::ClaudeCode, "claude", 91.0),
            ("%1", "stale", Harness::ClaudeCode, "claude", 92.0),
            ("%1", "a", Harness::Pi, "claude", 93.0),
            ("%9", "a", Harness::ClaudeCode, "claude", 94.0),
            // Synthetic normalized event from a different meter, not a Pi reader.
            ("%3", "c", Harness::Pi, "codex", 33.0),
        ] {
            cache.observe(
                &state,
                Source {
                    session: &SessionId::new(session),
                    pane,
                    harness,
                    meter,
                },
                now() + TimeDelta::seconds(percent as i64),
                vec![window(300, percent)],
            );
        }
        let snapshots = cache.list(now() + TimeDelta::minutes(5));
        let readings: Vec<_> = snapshots
            .iter()
            .map(|s| {
                (
                    s.profile.to_string(),
                    s.meter.as_str(),
                    s.windows[0].used_percent,
                )
            })
            .collect();
        assert_eq!(
            readings,
            [
                ("a_backup".into(), "claude", 22.0),
                ("codex".into(), "codex", 33.0),
                ("z_primary".into(), "claude", 11.0)
            ]
        );
    }

    #[test]
    fn unknown_invalid_and_duplicate_source_config_is_rejected() {
        assert!(PlanUsage::parse_config("").is_ok());
        for bad in [
            format!("{EXPLICIT}{EXPLICIT}"),
            EXPLICIT.replace("profile =", "profiel ="),
            EXPLICIT.replace("claude_code", "claud_code"),
            EXPLICIT.replace("\"claude\"", "\"cluade\""),
            EXPLICIT.replace("fixture/alpha", "alpha"),
            EXPLICIT.replace("a_backup", "a backup"),
            "[usage]\nsource = []".into(),
        ] {
            assert!(PlanUsage::parse_config(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn duplicate_redraw_does_not_refresh() {
        let cache = PlanUsage::default();
        let state = bindings();
        let id = SessionId::new("a");
        let source = Source {
            session: &id,
            pane: "%1",
            harness: Harness::ClaudeCode,
            meter: "claude",
        };
        cache.observe(
            &state,
            source,
            now(),
            vec![window(10080, 11.0), window(300, 25.0)],
        );
        cache.observe(
            &state,
            source,
            now() + TimeDelta::minutes(45),
            vec![window(300, 25.0), window(10080, 11.0)],
        );
        let stale = cache.list(now() + TimeDelta::minutes(60));
        assert_eq!(stale[0].observed_at, now());
        assert!(stale[0].stale);
        assert_eq!(stale[0].windows, [window(300, 25.0), window(10080, 11.0)]);
    }

    #[test]
    fn expired_and_missing_samples() {
        let cache = PlanUsage::default();
        let state = bindings();
        let id = SessionId::new("a");
        let source = Source {
            session: &id,
            pane: "%1",
            harness: Harness::ClaudeCode,
            meter: "claude",
        };
        assert!(cache.list(now()).is_empty());
        let reset = now() + TimeDelta::minutes(20);
        let short = PlanUsageWindow::from_epoch_seconds(300, 0.0, Some(reset.timestamp())).unwrap();
        cache.observe(&state, source, now(), vec![short, window(10080, 61.0)]);
        assert!(!cache.list(reset - TimeDelta::seconds(1))[0].stale);
        assert!(cache.list(reset)[0].stale);
        cache.observe(&state, source, reset, vec![window(10080, 65.0)]);
        // Stale receipt, empty data and invalid data cannot replace the latest sample.
        cache.observe(&state, source, now(), vec![window(300, 2.0)]);
        cache.observe(&state, source, reset + TimeDelta::minutes(1), Vec::new());
        cache.observe(
            &state,
            source,
            reset + TimeDelta::minutes(2),
            vec![PlanUsageWindow {
                duration_minutes: 0,
                ..window(300, 0.0)
            }],
        );
        let fresh = cache.list(reset + TimeDelta::minutes(59));
        assert_eq!(fresh[0].observed_at, reset);
        assert_eq!(fresh[0].windows, [window(10080, 65.0)]);
        assert!(!fresh[0].stale);
        assert!(cache.list(reset + TimeDelta::minutes(60))[0].stale);
        assert!(PlanUsage::default().list(reset).is_empty());
    }
}
