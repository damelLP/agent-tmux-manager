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
 * Idle recovery is conservative: an interactive input blocks snapshots
 * and idle dialog-resolution measurements until a genuine run/session
 * boundary. Input consumed or rejected by a later handler without a run
 * therefore withholds recovery until that boundary; reading isIdle=true
 * or a timer cannot clear the veto.
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
 * (`message_update`, `before_provider_request`, `turn_start`)
 * are deliberately not forwarded; the adapter's `to_lifecycle_event`
 * returns `None` for them anyway.
 *
 * `context` and `turn_end` are the exceptions: they fire per provider
 * request, before it and after its reply, and their frames carry pi's
 * session totals (`usageSnapshot`), which drive the TUI's cost +
 * context-percentage display. Their conversation bodies are omitted;
 * the envelope carries everything the adapter reads.
 */
const FORWARDED_EVENTS = [
	"session_start",
	"session_shutdown",
	"agent_start",
	// pi 1.5.2+: agent_end may precede recovery or continuation.
	"agent_settled",
	"tool_call",
	"tool_execution_end",
	"model_select",
	"input",
	"session_before_compact",
	"session_compact",
	"session_compact_failed",
	// `context` frames carry the session totals and the window that
	// drive the TUI's cost + context-percentage display. The full
	// conversation stays in pi: atmd reads the envelope's usage/model.
	"context",
	// `turn_end` fires after each reply is saved, including a turn's
	// last, which no later `context` would count. Sent with its
	// payload cut to `{}`.
	"turn_end",
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
 * Factory-owned transport: pi 1.5.2 reuses the imported module across
 * session factories. Open lazily; retry without another pi event. A
 * successful connection observes idle once, only after handshake/FIFO
 * backpressure clears. Already-written frames remain best-effort, without
 * acknowledgements/replay. The 256-frame cap bounds count, not bytes.
 */
function createTransport(getIdleEnvelope: () => unknown | undefined): {
	send(envelope: unknown): void;
	close(terminal?: unknown): void;
} {
	let alive = true;
	let activeSocket: net.Socket | null = null;
	let pendingSocket: net.Socket | null = null;
	let retryTimer: ReturnType<typeof setTimeout> | undefined;
	let connectTimer: ReturnType<typeof setTimeout> | undefined;
	let socketPaused = false;
	let snapshotPending = false;
	const outbox: string[] = [];
	let dropped = 0;

	function scheduleRetry(): void {
		if (!alive || retryTimer || activeSocket || pendingSocket) return;
		retryTimer = setTimeout(() => {
			retryTimer = undefined;
			if (alive) openSocket();
		}, 1000);
		retryTimer.unref();
	}
	function clearConnectTimer(): void {
		if (connectTimer) clearTimeout(connectTimer);
		connectTimer = undefined;
	}
	function disconnect(sock: net.Socket): void {
		if (activeSocket !== sock && pendingSocket !== sock) return;
		clearConnectTimer();
		activeSocket = pendingSocket = null;
		socketPaused = snapshotPending = false;
		sock.destroy();
		scheduleRetry();
	}
	function encode(envelope: unknown): string | undefined {
		try {
			return `${JSON.stringify({ protocol_version: { major: 1, minor: 0 }, type: "pi_event", data: envelope })}\n`;
		} catch (e) {
			logDebug(`stringify failed, dropping event: ${(e as Error).message}`);
			return undefined;
		}
	}
	function flushOutboxTo(sock: net.Socket): void {
		if (!alive || activeSocket !== sock || socketPaused) return;
		while (outbox.length > 0 && activeSocket === sock && !socketPaused) {
			const line = outbox.shift();
			if (line === undefined) break;
			try {
				socketPaused = !sock.write(line);
			} catch {
				outbox.unshift(line);
				disconnect(sock);
				return;
			}
		}
		if (activeSocket === sock && !socketPaused && outbox.length === 0 && snapshotPending) {
			snapshotPending = false; // A failed predicate is consumed too; never a heartbeat.
			try {
				const envelope = getIdleEnvelope();
				if (envelope !== undefined) send(envelope);
			} catch {
				// Stale/missing context must not interrupt pi or imply idle.
			}
		}
	}
	function openSocket(): void {
		if (!alive || activeSocket || pendingSocket) return;
		let sock: net.Socket;
		try {
			if (!fs.existsSync(SOCKET)) { scheduleRetry(); return; }
			sock = net.createConnection({ path: SOCKET });
		} catch {
			scheduleRetry();
			return;
		}
		pendingSocket = sock;
		sock.unref();
		connectTimer = setTimeout(() => {
			if (alive && pendingSocket === sock) disconnect(sock);
		}, 10000);
		connectTimer.unref();
		sock.on("connect", () => {
			if (!alive || pendingSocket !== sock) return;
			clearConnectTimer();
			pendingSocket = null;
			activeSocket = sock;
			snapshotPending = true;
			try {
				socketPaused = !sock.write(`${JSON.stringify({
					protocol_version: { major: 1, minor: 0 }, type: "connect", client_id: `pi-atm-${process.pid}`,
				})}\n`);
			} catch { disconnect(sock); return; }
			flushOutboxTo(sock);
		});
		sock.on("drain", () => {
			if (!alive || activeSocket !== sock) return;
			socketPaused = false;
			flushOutboxTo(sock);
		});
		sock.on("error", () => disconnect(sock));
		sock.on("close", () => disconnect(sock));
		// Drain unsolicited daemon responses; established idle sockets have no timeout.
		sock.on("data", () => {});
	}
	function send(envelope: unknown): void {
		if (!alive) return;
		const line = encode(envelope);
		if (line === undefined) return;
		outbox.push(line);
		if (outbox.length > 256) {
			outbox.shift();
			dropped++;
			if ((dropped & (dropped - 1)) === 0) logDebug(`outbox over cap: dropped ${dropped} event(s) total`);
		}
		if (activeSocket) flushOutboxTo(activeSocket);
		else if (!retryTimer) openSocket();
	}
	function close(terminal?: unknown): void {
		if (!alive) return;
		alive = false;
		// Never reopen/queue to deliver shutdown, or bypass older queued frames.
		if (terminal !== undefined && activeSocket && !socketPaused && outbox.length === 0) {
			const line = encode(terminal);
			if (line !== undefined) { try { activeSocket.write(line); } catch { /* best-effort */ } }
		}
		if (retryTimer) clearTimeout(retryTimer);
		retryTimer = undefined;
		clearConnectTimer();
		outbox.length = 0;
		activeSocket?.destroy();
		pendingSocket?.destroy();
		activeSocket = pendingSocket = null;
		socketPaused = snapshotPending = false;
	}
	return { send, close };
}

/**
 * Builds the PiEvent wire envelope from an event name and pi-emitted
 * payload. The shape matches `atm_pi_adapter::wire::RawPiEvent` so the
 * daemon-side `serde_json::from_value::<RawPiEvent>(data)` succeeds.
 */
function buildEnvelope(
	eventName: string,
	payload: unknown,
	sessionId: string | undefined,
	model?: ModelSnapshot,
	usage?: UsageSnapshot,
): unknown {
	return {
		event: eventName,
		payload,
		session_id: sessionId,
		pid: process.pid,
		tmux_pane: process.env.TMUX_PANE,
		model,
		usage,
	};
}

/** The usage pi puts on each assistant message, for that request alone. */
type Usage = {
	input?: number;
	output?: number;
	cacheRead?: number;
	cacheWrite?: number;
	totalTokens?: number;
	cost?: { total?: number };
};
type SessionEntry = {
	type?: string;
	message?: { role?: string; stopReason?: string; usage?: Usage };
};
/** The parts of pi's extension ctx that `usageSnapshot` reads. */
type UsageContext = {
	sessionManager?: { getEntries?: () => SessionEntry[]; getBranch?: () => SessionEntry[] };
	getContextUsage?: () => { tokens: number | null } | undefined;
};

/**
 * pi's session totals: `cost` in USD, `tokens` (input + output + cacheRead
 * + cacheWrite), and `contextTokens`, the active context (`null` when pi
 * doesn't know it, after a compaction until the next reply). Empty when
 * pi's session couldn't be read.
 */
type UsageSnapshot = { cost?: number; tokens?: number; contextTokens?: number | null };

/** A usage's input + output + cacheRead + cacheWrite, a missing part as 0. */
function sumTokens(u: Usage): number {
	return (u.input ?? 0) + (u.output ?? 0) + (u.cacheRead ?? 0) + (u.cacheWrite ?? 0);
}

/**
 * Sums usage over every assistant message in the session file, as pi's
 * footer does, so atm shows the cost pi shows. That's O(entries), like
 * each footer render; a running total would go wrong when /reload,
 * /resume or /fork load a session. The context size is the one the
 * footer's percentage uses. Never throws. When pi's session can't be
 * read it returns `{}`, so atmd still knows this pi-atm sends totals
 * and doesn't fall back to one request's cost.
 */
function usageSnapshot(ctx: UsageContext): UsageSnapshot {
	try {
		const entries = ctx.sessionManager?.getEntries?.();
		if (!entries) return {};
		let cost = 0;
		let tokens = 0;
		for (const entry of entries) {
			const usage = entry.type === "message" && entry.message?.role === "assistant" ? entry.message.usage : undefined;
			if (!usage) continue;
			cost += usage.cost?.total ?? 0;
			tokens += sumTokens(usage);
		}
		const contextUsage = ctx.getContextUsage?.();
		const contextTokens = contextUsage
			? contextUsage.tokens
			: lastContextTokens(ctx.sessionManager?.getBranch?.() ?? []);
		return { cost, tokens, contextTokens };
	} catch (e) {
		logDebug(`usage snapshot failed: ${(e as Error).message}`);
		return {};
	}
}

/**
 * The active context where `ctx.getContextUsage()` gives none (pi before
 * 0.49, or no model window): the last reply on the current branch that
 * wasn't aborted, as pi 0.43-0.48's footer picks it, counted as pi's
 * `calculateContextTokens` counts it.
 */
function lastContextTokens(branch: SessionEntry[]): number | undefined {
	const last = branch.findLast(
		(e) =>
			e.type === "message" &&
			e.message?.role === "assistant" &&
			e.message.usage !== undefined &&
			e.message.stopReason !== "aborted",
	);
	const u = last?.message?.usage;
	return u && (u.totalTokens || sumTokens(u));
}

/** The fields of a pi Model that atmd reads. */
type ModelSnapshot = { id: string; provider: string; contextWindow: number };

/**
 * Picks the fields atmd reads from a pi Model. Never forward a Model whole:
 * it can carry `headers`, and pi 0.62 and earlier (checked back to 0.43)
 * resolve auth into them (`Authorization: Bearer <apiKey>` for a provider
 * with `authHeader`). A field atmd needs later is added here on purpose.
 */
function pickModel(model: unknown): ModelSnapshot | undefined {
	if (!model || typeof model !== "object") return undefined;
	const { id, provider, contextWindow } = model as ModelSnapshot;
	return { id, provider, contextWindow };
}

/**
 * Picks model_select's `model` and `previousModel`; it is the only
 * forwarded payload that carries a pi Model. Copies rather than mutates:
 * pi hands the same payload to every extension.
 */
function withPickedModels(eventName: string, payload: unknown): unknown {
	if (eventName !== "model_select" || !payload || typeof payload !== "object") return payload;
	const p = payload as { model?: unknown; previousModel?: unknown };
	return { ...p, model: pickModel(p.model), previousModel: pickModel(p.previousModel) };
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
 *   3. After the last overlapping select closes, reports measured idle
 *      unless interactive input is pending; otherwise resumes work.
 *
 * If patching fails (different pi version, future API change), we log
 * and proceed — the rest of the extension still works.
 */
/**
 * Returns `true` iff `ctx.ui.select` was wrapped (or had been wrapped
 * by a previous call). Returns `false` when `ctx.ui.select` isn't yet
 * available — the caller should keep retrying on later events.
 */
type SelectObserver = { emit(event: string, payload: unknown): void; span(delta: number): number };
function patchUiSelectOnce(ctx: unknown, observer: SelectObserver): boolean {
	const ui = (ctx as { ui?: { select?: unknown; __atmPatched?: SelectObserver } }).ui;
	if (!ui || typeof ui.select !== "function") return false;
	if (ui.__atmPatched) { ui.__atmPatched = observer; return true; }
	const originalSelect = ui.select.bind(ui) as (
		title: string, options: string[], opts?: unknown,
	) => Promise<string | undefined>;
	ui.__atmPatched = observer;
	ui.select = async (title: string, options: string[], opts?: unknown): Promise<string | undefined> => {
		// Capture this call's owner: a late old finally cannot emit into a
		// replacement instance, even when pi reuses the shared UI object.
		const owner = ui.__atmPatched;
		if (owner?.span(1) === 1) owner.emit("atm_needs_input_open", { title });
		try {
			return await originalSelect(title, options, opts);
		} finally {
			if (owner?.span(-1) === 0) owner.emit("atm_needs_input_resolved", {});
		}
	};
	return true;
}

type LiveContext = UsageContext & {
	sessionManager?: { getSessionId?: () => string };
	model?: unknown;
	isIdle?: () => boolean;
	hasPendingMessages?: () => boolean;
};

export default function (pi: ExtensionAPI): void {
	logDebug(`pi-atm starting, socket=${SOCKET}`);

	// pi's session id, read on every event: pi 0.50–0.64 switch sessions
	// (/new, /resume, /fork) in place under this same instance, while pi
	// 0.65+ loads a new instance per session. One frame gets the wrong id:
	// an in-memory fork (`--no-session`) moves the session manager to the
	// new id before the old instance's `session_shutdown`. atmd still ends
	// that session by pid: a SessionEnd naming another id keeps the row only
	// when a `session_start` already moved the row off that id, and none
	// has here.
	let cachedSessionId: string | undefined;
	let uiPatched = false;
	let alive = true;
	let latestCtx: LiveContext | undefined;
	let nativeUiOpen = false;
	let selectDepth = 0;
	let inputPending = false;
	const transport = createTransport(() => {
		if (!alive || inputPending || nativeUiOpen || selectDepth > 0) return undefined;
		const ctx = latestCtx;
		if (ctx?.isIdle?.() !== true || ctx.hasPendingMessages?.() !== false) return undefined;
		const id = ctx.sessionManager?.getSessionId?.();
		if (typeof id !== "string" || id.length === 0 || id.startsWith("pending-")) return undefined;
		return buildEnvelope("atm_idle", {}, id, pickModel(ctx.model));
	});
	const selectObserver: SelectObserver = {
		emit(event, payload) {
			if (!alive) return;
			try {
				// Sample only at final closure. A pending input may still run next.
				const idle = event === "atm_needs_input_resolved" && !inputPending ? latestCtx?.isIdle?.() : undefined;
				transport.send(buildEnvelope(event, typeof idle === "boolean" ? { idle } : payload, cachedSessionId));
			} catch { /* best-effort UI reporting, including invalidated context */ }
		},
		span(delta) { if (alive) selectDepth = Math.max(0, selectDepth + delta); return selectDepth; },
	};
	for (const name of ["ui_prompt_start", "ui_prompt_end"] as const) {
		pi.on(name as any, (_payload: unknown, ctx: unknown) => {
			if (!alive) return;
			latestCtx = ctx as LiveContext;
			nativeUiOpen = name === "ui_prompt_start";
		});
	}

	for (const eventName of FORWARDED_EVENTS) {
		// pi.on takes a string-typed name. The TS type union it expects is
		// `ExtensionEvent`; our list contains both declared events
		// (`session_start`, `agent_start`, …) and undeclared-but-real
		// (`tool_call`). Cast to bypass the closed-union check; pi accepts
		// arbitrary names at runtime.
		// biome-ignore lint/suspicious/noExplicitAny: see comment above
		pi.on(eventName as any, async (payload: unknown, ctx: unknown) => {
			if (!alive) return;
			let terminal: unknown;
			try {
				if (eventName === "input" && (payload as { source?: unknown } | null)?.source === "interactive") inputPending = true;
				if (eventName === "session_start" || eventName === "agent_start" || eventName === "agent_settled") inputPending = false;
				latestCtx = ctx as LiveContext;
				logDebug(`event fired: ${eventName}`);
				// Duck-typed guard: if ctx lacks `getSessionId`, the event
				// still goes out, with the last id read.
				const ctxAny = ctx as LiveContext;
				cachedSessionId = ctxAny.sessionManager?.getSessionId?.() || cachedSessionId;
				// Only pi's model_select carries the model, and never at
				// startup, so each event's frame snapshots it (atmd takes the
				// context window from it).
				const model = pickModel(ctxAny.model);
				if (!uiPatched) {
					// Patch `ctx.ui.select` once, the first time we have a ctx
					// with `ui` bound. Early events can fire before the runner
					// attaches `ui`, so retry on each event until the patch
					// actually takes — `patchUiSelectOnce` returns `false`
					// when there's nothing to patch yet. Reporting belongs to
					// this instance, not the imported module or a later session.
					uiPatched = patchUiSelectOnce(ctx, selectObserver);
				}
				const counted = eventName === "context" || eventName === "turn_end";
				const compaction = eventName === "session_before_compact" || eventName === "session_compact" || eventName === "session_compact_failed";
				const reason = (payload as { reason?: string } | null)?.reason;
				// A queued turn can start before an earlier async failure handler finishes.
				// Never let that old manual failure clear the newer running turn.
				if (eventName === "session_compact_failed" && reason === "manual" && ctxAny.isIdle?.() === false) return;
				const envelope = buildEnvelope(
					eventName,
					// Keep conversation/preparation/summary bodies in pi. Only
					// manual compaction outcomes end work; auto outcomes await settled.
					compaction ? { reason }
						: counted || eventName === "agent_settled" ? {} : withPickedModels(eventName, payload),
					cachedSessionId,
					model,
					counted ? usageSnapshot(ctxAny) : undefined,
				);
				// Synchronous best-effort transport never gates pi's flow.
				if (eventName === "session_shutdown") terminal = envelope;
				else transport.send(envelope);
			} catch (e) {
				logDebug(`handler threw on ${eventName}: ${(e as Error).message}`);
			} finally {
				if (eventName === "session_shutdown") {
					alive = false;
					latestCtx = undefined;
					nativeUiOpen = inputPending = false;
					selectDepth = 0;
					transport.close(terminal);
				}
			}
			// Returning undefined means we don't block or modify pi's flow.
			return undefined;
		});
	}

	logDebug(`subscribed to ${FORWARDED_EVENTS.length} events`);
}
