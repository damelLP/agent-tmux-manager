/**
 * pi-atm — pi extension that forwards pi events to atmd for
 * monitoring in the ATM TUI.
 *
 * Symmetric counterpart to the bash `atm-hook` script that forwards
 * Claude Code hook events. This extension subscribes to the pi events
 * that drive vendor-neutral `LifecycleEvent` translation in
 * `atm-pi-adapter`, packages each event as `{event, payload, …}`, and
 * sends it to the atmd socket as `MessageType::PiEvent { data }`. The
 * daemon's connection layer parses the payload via `RawPiEvent::to_lifecycle_event()`
 * and applies the resulting LifecycleEvent to the session registry.
 *
 * Run:
 *   pi --extension /abs/path/to/pi-atm.ts
 *
 * Config (env):
 *   ATM_SOCKET   — daemon socket path (default: /tmp/atm.sock)
 *   ATM_DEBUG=1  — append debug log to /tmp/pi-atm.log
 *
 * NEVER throws or rejects — the contract is "if atmd is unavailable,
 * pi keeps working". All failures are swallowed (and optionally logged
 * when ATM_DEBUG=1).
 *
 * Tracks beads agent-tmux-manager-6dx.
 */

import * as fs from "node:fs";
import * as net from "node:net";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

const SOCKET = process.env.ATM_SOCKET || "/tmp/atm.sock";
const DEBUG = process.env.ATM_DEBUG === "1";

/**
 * pi events the atm daemon's pi adapter knows how to translate.
 *
 * Subscribing to *only* these (rather than every declared pi event)
 * keeps wire traffic low — the truly high-frequency events
 * (`message_update`, `before_provider_request`, `turn_start`/`turn_end`)
 * are deliberately not forwarded; the adapter's `to_lifecycle_event`
 * returns `None` for them anyway.
 *
 * `context` is the exception: it fires per provider request and
 * carries cumulative cost/tokens, which drive the TUI's cost +
 * context-percentage display, so we *do* forward it despite the
 * higher payload cost (see the inline note on its entry below).
 */
const FORWARDED_EVENTS = [
	"session_start",
	"session_shutdown",
	"agent_start",
	"agent_end",
	"tool_call",
	"tool_execution_end",
	"model_select",
	"input",
	"session_before_compact",
	// `context` carries cumulative cost/tokens that drive the TUI's
	// cost + context-percentage display. NOTE: pi attaches the full
	// conversation snapshot (every assistant message), so this is
	// the largest payload we forward — tens to hundreds of KB per
	// fire on long sessions. The daemon-side translator extracts
	// only the latest `usage.{cost.total, totalTokens}`, but the
	// wire payload is *not* small. The bounded outbox + drop-oldest
	// in send() exists precisely for this case.
	"context",
] as const;

function logDebug(msg: string): void {
	if (!DEBUG) return;
	try {
		fs.appendFileSync("/tmp/pi-atm.log", `${new Date().toISOString()} ${msg}\n`);
	} catch {
		// Logging is best-effort.
	}
}

/**
 * Persistent socket connection to atmd for the lifetime of this pi
 * process. Lazily opened on the first `send()`; reconnects automatically
 * if dropped (e.g. atmd restarts). One handshake per connection
 * instead of one per event keeps the daemon log quiet and avoids
 * thousands of short-lived connections in long pi sessions.
 *
 * `pendingSocket` is the in-flight connection while we wait for the
 * `connect` event; events that fire during the connect race are
 * buffered in `outbox` and flushed on connect.
 */
let activeSocket: net.Socket | null = null;
let pendingSocket: net.Socket | null = null;
let reconnectScheduled = false;
/**
 * `true` while the active socket's internal buffer has exceeded its
 * `highWaterMark` — i.e. `socket.write()` returned `false` and we
 * haven't yet seen the `'drain'` event.
 *
 * Without this, large `context` payloads against a slow atmd reader
 * silently grow Node's heap (write() always succeeds in the API
 * sense; the buffering happens inside Node). Honouring the
 * write-result keeps backpressure scoped to the bounded outbox
 * instead of spilling into the heap.
 */
let socketPaused = false;

/**
 * Drains as much of `outbox` as the socket can absorb without
 * triggering backpressure. Stops on the first `write` that returns
 * `false` — `'drain'` will resume the flush. Used by both `connect`
 * (to flush events queued during the handshake race) and `'drain'`
 * (to flush events queued while paused).
 */
function flushOutboxTo(sock: net.Socket): void {
	while (outbox.length > 0 && activeSocket === sock) {
		const line = outbox.shift();
		if (line === undefined) break;
		if (!sock.write(line)) {
			socketPaused = true;
			break;
		}
	}
}

/**
 * Bounded FIFO of outgoing event lines waiting for a usable socket.
 *
 * The cap exists so that an extended atmd outage in a long pi session
 * doesn't grow this array without limit. Pi context events can be
 * tens to hundreds of KB and fire frequently; an unbounded buffer
 * would let one wedged daemon eat the pi process's heap.
 *
 * Drop-oldest: when full, the *oldest* queued line is discarded so
 * the most recent state survives. Recent events are more useful for
 * "what is this session doing now" than stale ones from minutes ago,
 * and atmd's discovery scan reseeds the live registry on reconnect
 * regardless of what we managed to deliver.
 *
 * 256 is a small cap by design — at peak event rates this is seconds
 * of buffering, enough to ride out an atmd restart but not enough to
 * accumulate megabytes during a real outage.
 */
const OUTBOX_MAX = 256;
const outbox: string[] = [];
let outboxDropped = 0;

function enqueue(line: string): void {
	outbox.push(line);
	while (outbox.length > OUTBOX_MAX) {
		outbox.shift();
		outboxDropped++;
		// Log on each power-of-two boundary so chronic drops surface
		// without spamming the log on every overflow.
		if ((outboxDropped & (outboxDropped - 1)) === 0) {
			logDebug(`outbox over cap: dropped ${outboxDropped} event(s) total`);
		}
	}
}

function openSocket(): void {
	if (activeSocket || pendingSocket) return;
	if (!fs.existsSync(SOCKET)) {
		logDebug(`socket not found: ${SOCKET}`);
		return;
	}

	const sock = net.createConnection({ path: SOCKET });
	pendingSocket = sock;

	sock.on("connect", () => {
		logDebug("socket connected");
		pendingSocket = null;
		activeSocket = sock;
		socketPaused = false;
		// Send the protocol handshake once for this connection.
		const connectMsg = {
			protocol_version: { major: 1, minor: 0 },
			type: "connect",
			client_id: `pi-atm-${process.pid}`,
		};
		if (!sock.write(`${JSON.stringify(connectMsg)}\n`)) {
			socketPaused = true;
		}
		// Drain anything that queued while we were connecting.
		flushOutboxTo(sock);
	});

	sock.on("drain", () => {
		// Stale event from a closed socket: ignore.
		if (activeSocket !== sock) return;
		socketPaused = false;
		flushOutboxTo(sock);
	});

	sock.on("error", (err) => {
		logDebug(`socket error: ${err.message}`);
	});

	sock.on("close", () => {
		logDebug("socket closed");
		if (pendingSocket === sock) pendingSocket = null;
		if (activeSocket === sock) {
			activeSocket = null;
			socketPaused = false;
		}
		// Don't reconnect aggressively — let the next send() trigger
		// it, with a small backoff to avoid flapping.
		if (!reconnectScheduled) {
			reconnectScheduled = true;
			setTimeout(() => {
				reconnectScheduled = false;
			}, 1000);
		}
	});

	// Daemon may keep this socket open indefinitely; no read timeout.
	// We only write; if the daemon disconnects, `close` fires.
	sock.on("data", () => {
		// We don't expect responses to fire-and-forget pi events. Drain
		// any frames the daemon sends back.
	});
}

/**
 * Sends a single PiEvent envelope to atmd.
 *
 * Fire-and-forget: pi's flow doesn't wait. If the socket is open the
 * event lands immediately; if it's mid-connect or down, the event is
 * queued in `outbox` and flushed on connect (or dropped if connect
 * fails — daemon observability is best-effort, never gate pi's flow
 * on it).
 */
function send(envelope: unknown): void {
	const dataMsg = {
		protocol_version: { major: 1, minor: 0 },
		type: "pi_event",
		data: envelope,
	};
	// JSON.stringify can throw on circular references or unsupported
	// values (BigInt, etc). Pi event payloads shouldn't contain
	// either, but the wrapping handler in the event subscription
	// promises send() never throws — honour that here so a
	// surprising payload doesn't crash pi's flow.
	let line: string;
	try {
		line = `${JSON.stringify(dataMsg)}\n`;
	} catch (e) {
		logDebug(`stringify failed, dropping event: ${(e as Error).message}`);
		return;
	}

	if (activeSocket) {
		if (socketPaused) {
			// Socket is up but back-pressured (a previous write exceeded
			// `highWaterMark` and `'drain'` hasn't fired yet). Queue
			// instead of writing — `flushOutboxTo` will drain on drain.
			enqueue(line);
			return;
		}
		try {
			// `socket.write` returns `false` when the internal buffer
			// is past `highWaterMark`. Subsequent writes still
			// "succeed" but spill into Node's heap; the convention is
			// to stop writing until `'drain'` fires. With pi's large
			// `context` payloads against a slow atmd reader, ignoring
			// this would let one wedged daemon eat the pi process's
			// heap.
			if (!activeSocket.write(line)) {
				socketPaused = true;
			}
			return;
		} catch (e) {
			logDebug(`write threw: ${(e as Error).message}`);
			activeSocket = null;
			socketPaused = false;
		}
	}

	if (pendingSocket) {
		// Queue until connect fires.
		enqueue(line);
		return;
	}

	if (!reconnectScheduled) {
		enqueue(line);
		openSocket();
	}
	// If reconnect is back-off-pending, drop on the floor — the next
	// send() after the back-off clears will trigger a fresh
	// openSocket() and the bounded outbox covers buffering from then.
}

/**
 * Builds the PiEvent wire envelope from an event name and pi-emitted
 * payload. The shape matches `atm_pi_adapter::wire::RawPiEvent` so the
 * daemon-side `serde_json::from_value::<RawPiEvent>(data)` succeeds.
 */
function buildEnvelope(eventName: string, payload: unknown, sessionId: string | undefined): unknown {
	return {
		event: eventName,
		payload,
		session_id: sessionId,
		pid: process.pid,
		tmux_pane: process.env.TMUX_PANE,
	};
}

/**
 * Wraps `ctx.ui.select` so any extension that opens a permission /
 * elicitation dialog (most often pi-amplike's bash permission gate)
 * gets atm visibility for free.
 *
 * Pi's `ExtensionContext.ui` is a getter that returns the runner's
 * shared `uiContext` — a single object across all extensions. Patching
 * its `select` method affects every extension. We do this once, on the
 * first event we receive (when the runner is fully initialized).
 *
 * The patch:
 *   1. Emits `atm_needs_input_open` to atmd → session shows "Needs input"
 *   2. Awaits the original `select` (pi blocks; user picks)
 *   3. Emits `atm_needs_input_resolved` → session resumes work
 *
 * If patching fails (different pi version, future API change), we log
 * and proceed — the rest of the extension still works.
 */
/**
 * Returns `true` iff `ctx.ui.select` was wrapped (or had been wrapped
 * by a previous call). Returns `false` when `ctx.ui.select` isn't yet
 * available — the caller should keep retrying on later events.
 */
function patchUiSelectOnce(
	ctx: unknown,
	getSessionId: () => string | undefined,
): boolean {
	const ctxAny = ctx as { ui?: { select?: unknown; __atmPatched?: boolean } };
	const ui = ctxAny.ui;
	if (!ui || typeof ui.select !== "function") {
		logDebug("patchUiSelectOnce: no ui.select to patch");
		return false;
	}
	if (ui.__atmPatched) return true; // already wrapped
	const originalSelect = ui.select.bind(ui) as (
		title: string,
		options: string[],
		opts?: unknown,
	) => Promise<string | undefined>;

	// Note: `getSessionId` is a getter, not a value. It is read on every
	// `select` call so synthetic events still carry the correct session
	// id even when patching happens before `currentSessionId` is set
	// (e.g. before the first `session_start` event fires).
	ui.select = async function patchedSelect(
		title: string,
		options: string[],
		opts?: unknown,
	): Promise<string | undefined> {
		logDebug(`ui.select intercepted: ${title.slice(0, 80)}`);
		void send(buildEnvelope("atm_needs_input_open", { title }, getSessionId()));
		try {
			return await originalSelect(title, options, opts);
		} finally {
			void send(buildEnvelope("atm_needs_input_resolved", {}, getSessionId()));
		}
	};
	ui.__atmPatched = true;
	logDebug("patchUiSelectOnce: ui.select wrapped");
	return true;
}

export default function (pi: ExtensionAPI): void {
	logDebug(`pi-atm starting, socket=${SOCKET}`);

	// `sessionManager.currentSessionId` only becomes available after
	// `session_start` fires. Cache it from the first event we see.
	let cachedSessionId: string | undefined;
	let uiPatched = false;

	for (const eventName of FORWARDED_EVENTS) {
		// pi.on takes a string-typed name. The TS type union it expects is
		// `ExtensionEvent`; our list contains both declared events
		// (`session_start`, `agent_start`, …) and undeclared-but-real
		// (`tool_call`). Cast to bypass the closed-union check; pi accepts
		// arbitrary names at runtime.
		// biome-ignore lint/suspicious/noExplicitAny: see comment above
		pi.on(eventName as any, async (payload: unknown, ctx: unknown) => {
			try {
				logDebug(`event fired: ${eventName}`);
				if (!cachedSessionId) {
					// Best-effort session id discovery via duck-typed access.
					const ctxAny = ctx as { sessionManager?: { currentSessionId?: string } };
					cachedSessionId = ctxAny.sessionManager?.currentSessionId;
				}
				if (!uiPatched) {
					// Patch `ctx.ui.select` once, the first time we have a ctx
					// with `ui` bound. Early events can fire before the runner
					// attaches `ui`, so retry on each event until the patch
					// actually takes — `patchUiSelectOnce` returns `false`
					// when there's nothing to patch yet. Pass a getter so the
					// patched closure reads the *current* `cachedSessionId`,
					// even if patching ran before the id was discovered.
					uiPatched = patchUiSelectOnce(ctx, () => cachedSessionId);
				}
				const envelope = buildEnvelope(eventName, payload, cachedSessionId);
				// Fire-and-forget: `send()` is synchronous and returns
				// `void` — it either writes to the live socket, queues
				// onto the bounded outbox, or drops on the floor when a
				// reconnect back-off is in effect. None of those gate
				// pi's flow.
				send(envelope);
			} catch (e) {
				logDebug(`handler threw on ${eventName}: ${(e as Error).message}`);
			}
			// Returning undefined means we don't block or modify pi's flow.
			return undefined;
		});
	}

	logDebug(`subscribed to ${FORWARDED_EVENTS.length} events`);
}
