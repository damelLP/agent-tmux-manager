//! Pure organisation configuration, events and state.
//!
//! Every change follows **decide → append → apply**. User requests and daemon
//! reports are both [`OrgCommand`]s. Decide is nonmutating policy; apply folds
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
//! | Adopt | unknown project; another holder; own different pane | same pane and session | ready with session, else starting |
//! | PaneCreated | unknown agent; not starting; another holder; own different pane | same recorded pane | record pane |
//! | SessionBound | unknown agent; down; no pane | ready with same session | ready, including stuck recovery/manual clear |
//! | SessionLost | unknown agent | starting without session; clearing; down | starting, session removed |
//! | Clear | unknown agent; not ready | — | clearing, session removed |
//! | Stuck | unknown agent; down | stuck with identical reason | stuck with reason |
//! | Down | unknown agent | already down | down, release pane/session |
//! | Respawn | unknown project/agent; mismatched role | — | down unless already down, then up |
//!
//! Project registration replaces a moved root; identical root is a no-op.
//! Send requires known agent addresses and refuses the system recipient. Owner
//! messages are delivered immediately; all other messages queue. DeliveryAttempted
//! requires queued and a positive increasing attempt (an identical delivering
//! report is a no-op). Delivered/DeliveryFailed require delivering; terminal
//! delivered/failed messages are no-ops, as is an identical retryable failure
//! already queued. Read requires delivered messages addressed to the caller;
//! an entirely read/empty batch is a no-op. Summary reports replace complete
//! snapshots, require a known agent when bound, and deduplicate identical facts.
//! Control audits always emit and do not change organisation state.
//!
//! # Data and limits
//!
//! Roles name **built-in harnesses only**: user-defined harnesses from the ATM
//! config are not supported yet. TOML rejects unknown keys; persisted event JSON
//! accepts additive fields. Role snapshots flatten their config. See [`OrgEvent`]
//! for kind/body encoding; summary costs are exact integer microdollars.
//!
//! Every message and the latest summary of every session stay in memory, with
//! **no compaction yet**. Queries order agents/projects/messages by ID and an
//! agent's sessions by start time then session ID. Costs sum latest snapshots
//! once per session (None if no cost was reported), using Money's saturation.
//! Delivery timestamps and first read time come from apply's supplied timestamp.

mod command;
mod config;
mod event;
mod ids;
mod replay;
mod state;
pub use command::*;
pub use config::*;
pub use event::*;
pub use ids::*;
pub use replay::*;
pub use state::*;
