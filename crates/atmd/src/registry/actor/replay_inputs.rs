//! Test-only record/replay of scalar samples and keyed process observations.
//! Missing/mismatched/unused inputs panic in tests, never fall back to the host.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atm_core::{Harness, SessionId};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::registry::inputs::{Clock, ProcessInfo, SystemClock, SystemProcessInfo};

type Answers = BTreeMap<String, VecDeque<Value>>;

pub(super) struct InputTape {
    pub(super) origin: Instant,
    pub(super) recording: bool,
    clocks: Mutex<VecDeque<Value>>,
    processes: Mutex<Answers>,
}

impl InputTape {
    pub(super) fn record() -> Arc<Self> {
        Arc::new(Self {
            origin: Instant::now(),
            recording: true,
            clocks: Mutex::default(),
            processes: Mutex::default(),
        })
    }

    pub(super) fn replay(origin: Instant, v: &Value) -> Arc<Self> {
        Arc::new(Self {
            origin,
            recording: false,
            clocks: Mutex::new(serde_json::from_value(v["clocks"].clone()).unwrap()),
            processes: Mutex::new(serde_json::from_value(v["processes"].clone()).unwrap()),
        })
    }

    fn clock(&self, kind: &str, real: impl FnOnce() -> Value) -> Value {
        let mut clocks = self.clocks.lock().unwrap();
        if self.recording {
            let v = real();
            clocks.push_back(json!({"kind":kind, "value":v}));
            v
        } else {
            let sample = clocks.pop_front().expect("missing recorded clock sample");
            assert_eq!(sample["kind"], kind, "mismatched clock sample");
            sample["value"].clone()
        }
    }

    fn process<T: Serialize + DeserializeOwned>(
        &self,
        query: String,
        real: impl FnOnce() -> T,
    ) -> T {
        let mut processes = self.processes.lock().unwrap();
        if self.recording {
            let answer = real();
            processes
                .entry(query)
                .or_default()
                .push_back(serde_json::to_value(&answer).unwrap());
            answer
        } else {
            let answer = processes
                .get_mut(&query)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| panic!("missing process answer {query}"));
            serde_json::from_value(answer).unwrap()
        }
    }

    pub(super) fn take(&self) -> Value {
        json!({"clocks": std::mem::take(&mut *self.clocks.lock().unwrap()),
            "processes": std::mem::take(&mut *self.processes.lock().unwrap())})
    }

    pub(super) fn assert_consumed(&self) {
        assert!(
            self.clocks.lock().unwrap().is_empty(),
            "unused clock samples"
        );
        assert!(
            self.processes
                .lock()
                .unwrap()
                .values()
                .all(VecDeque::is_empty),
            "unused process answers"
        );
    }
}

impl Clock for InputTape {
    fn utc_now(&self) -> DateTime<Utc> {
        serde_json::from_value(self.clock("utc", || json!(SystemClock.utc_now()))).unwrap()
    }
    fn instant_now(&self) -> Instant {
        let nanos: u64 = serde_json::from_value(self.clock("instant", || {
            json!(u64::try_from(
                SystemClock
                    .instant_now()
                    .duration_since(self.origin)
                    .as_nanos()
            )
            .unwrap())
        }))
        .unwrap();
        self.origin + Duration::from_nanos(nanos)
    }
}

impl ProcessInfo for InputTape {
    fn start_time(&self, pid: u32) -> Option<u64> {
        self.process(format!("start:{pid}"), || SystemProcessInfo.start_time(pid))
    }
    fn alive(&self, pid: u32, start: u64) -> bool {
        self.process(format!("alive:{start}:{pid}"), || {
            SystemProcessInfo.alive(pid, start)
        })
    }
    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        self.process(format!("cwd:{pid}"), || SystemProcessInfo.cwd(pid))
    }
    fn parent(&self, pid: u32, harness: Harness) -> Option<SessionId> {
        self.process(format!("parent:{harness:?}:{pid}"), || {
            SystemProcessInfo.parent(pid, harness)
        })
    }
}
