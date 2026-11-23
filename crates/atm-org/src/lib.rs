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
//! not after `now`. This policy says whether a decision may be taken
//! provisionally; the away policy below says when one is.
//!
//! [`DecisionCommand::Relay`] first requires the designated full agent ID.
//! Pending decisions may relay any listed option, even without a recommendation,
//! only if undoable and not a kind that always waits in the away table. The fact
//! is provisional `by: owner` with `relayed_by` naming the recorder; self-relays
//! have the same policy. A same-option provisional retry writes nothing, whatever
//! its provenance; a different option returns [`DecisionRefusal::AlreadyTaken`].
//! Closed decisions refuse. The owner's Resolve confirms or overrules a
//! provisional choice, retaining the relay fact and using the existing cap and
//! frozen redo path. The daemon also lets the designated relay confirm the
//! same provisional option in the owner's words. This uses the same final
//! Resolve; only the owner can overrule, and only while still provisional.
//!
//! # Away policy
//!
//! The away switch arrives as [`OrgCommand::Owner`]: only the owner sets away
//! and back, repeats write nothing, and `owner.away` records each project's
//! stacking cap (at least one). While he is away the daemon reports every
//! pending decision with [`OwnerCommand::ApplyAway`], which puts it through the
//! away table in one decide step: a detail is recorded for its requester, a
//! change of intent that can be undone is taken provisionally by `system`,
//! both on the recommended option; anything else waits, as everything does
//! while he is present. The stacking cap then holds `Assign` of an open task
//! whose `rests_on` names a provisional change of intent until the owner has
//! answered it. Both tables and their limits are in the away module's docs.
//!
//! Independently, only the owner designates or clears one full agent ID with
//! [`OwnerCommand::SetRelay`]. `owner.relay_set` persists that slot; repeating
//! the same designation writes nothing. Away and back never change it.
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
//! # Channels
//!
//! A **post** is one fact in a channel (`channel.posted`, id `m<seq>`, in the
//! message id space). A **wake** is one delivery of it to one address: an ordinary
//! `msg.sent` row whose `post` names the post, so delivery, receipts, ack and
//! retry run on it unchanged. A [`ChannelId`] is a project or a direct pair of
//! addresses. Channels and membership are derived, never stored: every older
//! `msg.sent` without `post` reads as a post of its direct channel, so old logs
//! need no migration. An untagged post waits for one `channel.routed` answer;
//! `channel.seen` moves a reader's marker forward. The fold's rules are in
//! the module docs of `src/channel.rs`; [`channel_query`] reads the fold.
//!
//! Rollback rule for these bodies: open tags are strings and every channel enum
//! has a read-only catch-all, so a promoted binary decodes anything a later one
//! writes into the three kinds. Decoding is not the whole rule: it still refuses
//! a record that names something it does not know, and that disables the
//! organisation. So a later event kind must not create anything a `channel.*`
//! record or a wake row then names (a post, a thread root, a decision); such a
//! record needs a new kind too. [`MessageKind`] and [`Address`] have no catch-all:
//! a new message kind or address form must not appear inside `channel.posted` or
//! `msg.sent` until a catch-all for it exists in a promoted binary; until then a
//! new form arrives as a new event kind.
//!
//! # Channel policy
//!
//! Channel commands arrive as [`OrgCommand::Channel`]. Their facts depend on
//! state, so [`channel_policy`] builds each one and checks it against the
//! fold's references before emitting it. A post's wake rows and acks by post id
//! use the existing [`OrgCommand::SendMessage`] and [`OrgCommand::ReadMessages`].
//! Checks run in the order listed.
//!
//! | Command | Check | Result |
//! |---|---|---|
//! | Post | unknown author; later-form channel or decision step; client key outside 1 to 64 bytes | refuses |
//! | | a client key this author used, or a decision step already posted | no event; the queries name the first post |
//! | | place: a listed channel (an internal post may open a direct channel: the decision bridge's question in the asker's DM with the owner, which wakes him unless it was asked before the daemon started; its tags, not the target, say whom it wakes); `Direct(to)`, the pair of author and `to` (neither the author nor `system`); `Reply`, the thread root of a post, reply or wake | refuses an unlisted channel, a bad recipient, an unknown post |
//! | | member: in a project channel the owner, `system` and seats that are not down (any seat of the project when the daemon marks the post internal); in a direct channel its two parties; in any channel `system` posting a decision step internally (design 12 asks in the asker's DM with the owner when its project channel is not listed, and 6.1 step 3 would refuse `system` there) | refuses anyone else |
//! | | tags: an unresolved inline `@name` stays text; an unresolved strict tag refuses. Internal posts scan only the strict tags so copied text cannot add mentions or wakes | refuses an unresolved strict tag |
//! | | otherwise | `channel.posted`, waking the direct party, then the tagged (a group's seats that are not down), never the author |
//! | Route | later router kind or stopgap cause; reason over 256 bytes; unknown post; post with `route: false` | refuses |
//! | | an answer after the first: the same `by` and address set | no event; any other answer refuses (conflict) |
//! | | an address that is the author, `system`, or no member (project: the owner or a seat that is not down; direct: the other party) | refuses |
//! | | otherwise | `channel.routed`, duplicates dropped; the empty set is final |
//! | Wake row (`SendMessage` with `post`) | a row for (post, recipient) exists | no event |
//! | | sender not the author; a recipient neither the post nor its routing answer wakes; not a copy of the post (kind, body, attachments, task, no notice) | refuses |
//! | ReadMessages | a post id: the reader's own row for it, or refuses | then today's ack, naming rows |
//! | Seen | later-form channel; `through` not a post of the channel; unknown reader (by the fact's reference check) | refuses |
//! | | otherwise | `channel.seen` only when the marker advances; for the owner, also `msg.read` for his unread rows whose post is at or before `through`, advance or not |
//!
//! A post is routed (`route: true`) when it wakes nobody, its author is not
//! `system`, `no_route` is unset, and someone else in the channel can be woken.
//! The owner is a member of every project channel, so route is false only for
//! `system`, `no_route`, the owner when every seat of the project is down, and
//! direct channels with `system`.
//!
//! The copy rule for wake rows is strict on purpose. A later producer whose row
//! must differ from its post (a decision reply, say) relaxes it with a named
//! reason.
//!
//! Until a router is attached, [`channel_policy::stopgap_wake`] answers every
//! routed post. First match wins, and only addresses `Route` accepts qualify:
//! (1) in a direct channel, the other party; (2) a thread reply wakes one
//! target: the root's author, else the latest earlier replier other than this
//! one, else the root's single wake, else nobody; (3) the owner's top-level post
//! wakes the project's lead; (4) the lead's wakes every other seat that is
//! starting, ready or clearing; (5) any other seat's wakes the lead; otherwise
//! nobody. [`channel_policy::route_step`] says when a pending post falls to the
//! stopgap: at once when routing is off or no router is there, at the end of a
//! grace, or one timeout after the later of the post and the router's attach.
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

pub mod auto_handover;
mod away;
pub mod budget;
mod channel;
pub mod channel_policy;
pub mod channel_query;
mod command;
mod config;
mod cost;
mod decision;
mod decision_due;
mod decision_impact;
mod decision_redo;
pub mod delivery;
pub mod department;
mod event;
pub mod idle;
mod ids;
mod intent;
pub mod mention;
pub mod message_query;
pub mod reminder;
mod replay;
mod report;
mod return_digest;
mod state;
mod task;
pub use away::*;
pub use channel::*;
pub use channel_policy::{ChannelCommand, ChannelRefusal, PostMutation, PostRequest, PostTarget};
pub use command::*;
pub use config::*;
pub use cost::{TaskUsageObservation, TaskUsageObserved};
pub use decision::*;
pub use decision_due::*;
pub use decision_impact::*;
pub use decision_redo::*;
pub use event::*;
pub use ids::*;
pub use intent::*;
pub use replay::*;
pub use report::*;
pub use return_digest::*;
pub use state::*;
pub use task::*;
