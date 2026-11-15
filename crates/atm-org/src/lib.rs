//! Pure organisation configuration, events and state.
//!
//! Every change follows **decide → append → apply**, serialized for one command
//! at a time by one daemon actor owning OrgState (or one lock across all three
//! steps). Deciding on concurrent snapshots is unsafe: Adopt(p/other, %1) and
//! PaneCreated(p/lead, %1) can both pass against the same free pane, but logging
//! both makes the second apply fail and breaks later replay. Do not append a
//! second command's events before the first command's events have been applied.
//! User requests and daemon reports are both [`OrgCommand`]s. Decide is nonmutating policy; apply folds
//! committed facts without enforcing transition policy, so recovered late facts
//! and older logs remain applicable. Apply errors (invalid IDs, unknown named
//! entities, duplicate message IDs, occupied panes) leave state unchanged.
//! Time and event sequences are inputs; this crate performs no I/O.
//!
//! # Command policy
//!
//! All commands validate IDs and required references, including duplicate reports.
//! Agent commands use this matrix (project/role checks apply to Up and Respawn):
//!
//! | Command | Refusal | No event | Otherwise |
//! |---|---|---|---|
//! | Up | unknown project, mismatched role; starting/ready/clearing; stuck with pane | — | starting, fresh role snapshot |
//! | Adopt | unknown project; another holder; own different pane | same pane/session and resulting status, unless binding a session with an exclusion | ready with session (reset exclusion), else starting (retain exclusion) |
//! | PaneCreated | unknown agent; another holder; own different pane; new pane while not starting | same recorded pane, in any status | record pane |
//! | SessionBound | unknown agent; down; no pane | excluded session; ready with same session | ready, including stuck recovery/manual clear; reset exclusion |
//! | EditorReady | unknown agent; down; no pane | ready, with or without session | starting/clearing/stuck → ready; session unchanged, reason cleared |
//! | SessionLost | unknown agent | starting, stuck or ready without session; clearing; down | starting, session removed; exclusion retained |
//! | Revalidate | unknown agent; neither ready nor starting with a recorded session | starting with a recorded session | ready → starting; pane, session and exclusion retained; reason cleared |
//! | Clear | unknown agent; not ready | — | clearing, session removed and excluded; retain exclusion if no session |
//! | Stuck | unknown agent; down | stuck with identical reason | stuck with reason |
//! | Down | unknown agent | already down | down, release pane/session/exclusion |
//! | Respawn | unknown project/agent; mismatched role | — | down unless already down, then up |
//!
//! EditorReady reports a positively observed fresh editor in the recorded pane.
//! The caller must observe it after the operation it completes (clear, up or
//! respawn), or observe a safe idle editor during restart/adopt revalidation,
//! and submit the report in the same serialized step owning the agent.
//! Serializing decide/append/apply alone does not protect an earlier observation.
//! Known limits, confirmed with command sequences:
//! - Clear → observe editor → Respawn → PaneCreated on a replacement pane →
//!   queued EditorReady incorrectly completes readiness on the replacement.
//! - Clear → EditorReady → Clear again → late EditorReady from the first clear
//!   incorrectly completes the second clear.
//!
//! The command carries no pane/generation check. One can be added later without
//! changing the log, because commands are not logged; today callers must prevent
//! these interleavings by serializing observation with the report.
//!
//! Up and session-less Adopt start without a session; Clear and SessionLost
//! remove it. Revalidate instead preserves the recorded binding while suspending
//! readiness: Starting can now have a session. The caller uses it under an Observe
//! reservation on restart/adopt when readiness must be checked again. Busy or
//! unknown observations remain Starting until new evidence; silence alone is not
//! a reason to mark Stuck. EditorReady restores readiness without discarding that
//! binding; a new SessionBound still replaces it by the existing policy.
//! EditorReady after Up/Clear/SessionLost remains session-less. Stuck recovery keeps
//! any session already bound, and a late report on ready is a no-op. Applying
//! the fact preserves session, pane and exclusion; replay does not revalidate
//! the screen. The caller must revalidate it on startup. A ready agent without
//! a session ignores SessionLost: callers must detect a harness exiting from
//! the pane itself, not from a missing registry row.
//!
//! Clear retains one pre-clear session ID to ignore obsolete SessionBound
//! reports, even before EditorReady. Repeated clears without a new binding keep
//! that exclusion; EditorReady, Revalidate, SessionLost and Stuck retain it. A new binding,
//! Down or fresh Up resets it. Adopt with a session explicitly overrides the
//! exclusion; session-less Adopt repairs status to starting while retaining it.
//! Routine session observations must use SessionBound, not Adopt. Two known
//! limits follow from a single pending exclusion:
//! - A genuine later report of the pre-clear ID (an in-process resume) is also
//!   ignored until a new ID binds, Adopt explicitly binds a session, or Down
//!   releases the agent. An Adopt naming the resumed session is the override.
//! - Clear → SessionBound(new ID) → SessionBound(old ID) binds the old ID again:
//!   the new binding has already cleared the exclusion. This is not a permanent
//!   blacklist; callers must avoid replaying stale observations after a binding.
//!
//! Committed binding facts still fold regardless of today's exclusion policy,
//! so older logs remain replayable.
//!
//! Project registration is a no-op for the same root, but a different root is a
//! conflict unless `move_root: true`: the daemon must first check that the old
//! root no longer holds this project. [`OrgState::free_project_id`] chooses a
//! free folder slug or numeric suffix without reserving it. Project and agent
//! IDs start with a letter/digit, avoiding command-line flag ambiguity.
//! Send requires known agent addresses and refuses the system recipient. Owner
//! messages are delivered immediately; all other messages queue. DeliveryAttempted
//! is a no-op when delivered/failed or an identical attempt is already delivering;
//! otherwise it requires queued and a positive increasing attempt. Delivered
//! records confirmations from queued, delivering or failed, and is a no-op only
//! when already delivered (unknown messages refuse). DeliveryFailed requires
//! delivering, or queued for a final failure (giving up on a held message).
//! Terminal messages and identical retryable failures already queued are no-ops.
//! Read requires delivered messages addressed to the caller;
//! an entirely read/empty batch is a no-op. Summary reports replace complete
//! snapshots, require a known agent when bound, and deduplicate identical facts.
//! They accept ended before started: wall-clock steps must not drop last cost.
//! Control audits refuse an empty pane; otherwise they always emit and do not
//! change organisation state. Handover audits (SaveRequested, Saved) need a
//! known agent and message; they too always emit and change no state.
//!
//! # Decision policy
//!
//! A decision is a question with listed options put to the owner; see the
//! decision module's types for its fields and derived class. Its commands
//! arrive as [`OrgCommand::Decision`]. Rows are the six states; "no event" is
//! a repeat of what already happened.
//!
//! | State | TakeProvisionally | Resolve | Expire | Withdraw |
//! |---|---|---|---|---|
//! | pending | taken provisionally; refuses a decision that may not be undone, and any option but the recommended one | resolved | expired once `now` reaches the deadline; refuses without a deadline or before it | withdrawn |
//! | taken provisionally | no event for the same option | resolved for the provisional option, overruled for another | refuses | refuses |
//! | resolved, overruled | refuses | no event for the identical answer, else refuses | refuses | refuses |
//! | expired | refuses | refuses | no event | refuses |
//! | withdrawn | refuses | refuses | refuses | no event |
//!
//! Before the state is looked at, Resolve refuses anyone but the owner, and
//! Withdraw refuses anyone but the requester and the owner. On a pending or
//! provisional decision, Resolve refuses an option that is not listed.
//! Request accepts only a known agent or `system` as requester and a known
//! [`DecisionKind`]. It refuses an empty title, fewer than two options, an empty
//! or repeated option, a recommendation that is not an option, and a deadline
//! not after `now`. Whether the owner is away is not known here: the caller decides
//! when to take a decision provisionally, this policy whether it may.
//!
//! # Tasks
//!
//! [`TaskCommand`] is wrapped in [`OrgCommand::Task`] and has its own policy:
//! the transition table, who may ask, and the review and measure rules are in
//! the module docs at the top of `task.rs`. `tests/task_golden.rs` states the
//! body of every `task.*` event. Every task stays in memory, in creation order.
//!
//! # Delivery policy
//!
//! [`delivery::next_delivery`] is pure: the executor supplies observations and
//! continuity clocks, serializes per-pane attempts, and consumes each step once.
//! The module documents hold resets and reconstruction of the independent agent
//! readiness clock. A stale-row draft/dialog is reported after three guard polls;
//! unknown observations keep the overall readiness timeout. `send_now` still
//! waits for a positively empty editor during a fresh busy turn.
//!
//! Queued retries reserve their recipient agent via [`delivery::delivery_in_flight`],
//! following pane moves without holding a different agent that reuses the old pane.
//! Blocked emits no event and retains that reservation until receipt or owner
//! give-up. `Wait(OtherDelivery)` is deliberately unbounded: the blocked message's
//! owner notice must list the messages queued behind it, not silently hide them.
//!
//! # Handover policy
//!
//! [`idle::handover_step`] is pure: from an agent's context share, idle time
//! and what it waits for, and the limits passed in for its harness and model,
//! it answers nothing, flag, save or clear. It never refuses a handover.
//!
//! # Compatibility direction
//!
//! A newer daemon must read every older log. An older daemon may fail on a newer
//! log: a new message kind or address form, or a known event referring to an entity
//! created by a skipped unknown kind. On such replay errors the daemon must disable
//! organisation methods, not fail to start. Unknown decision kinds fold as
//! [`DecisionKind::Unknown`] (the original word is lost in the fold only; the log
//! keeps it); new requests refuse it.
//!
//! # Data and limits
//!
//! Roles name **built-in harnesses only**: user-defined harnesses from the ATM
//! config are not supported yet. TOML rejects unknown keys; persisted event JSON
//! accepts additive fields. Role snapshots flatten their config. See [`OrgEvent`]
//! for kind/body encoding; summary costs are exact integer microdollars.
//! Known decoder limits: serde accepts positional arrays for some bodies and
//! nested structs; unknown harness strings decode as Harness::Unknown (the raw
//! name is not retained). The daemon is the log's only writer and emits objects.
//!
//! Every message and the latest summary of every session stay in memory, with
//! **no compaction yet**. Queries order agents/projects/messages by ID and an
//! agent's sessions by start time then session ID. Costs sum latest snapshots
//! once per session (None if no cost was reported), using Money's saturation.
//! Delivery timestamps and first read time come from apply's supplied timestamp.

mod command;
mod config;
mod decision;
pub mod delivery;
mod event;
pub mod idle;
mod ids;
mod replay;
mod state;
mod task;
pub use command::*;
pub use config::*;
pub use decision::*;
pub use event::*;
pub use ids::*;
pub use replay::*;
pub use state::*;
pub use task::*;
