import assert from "node:assert/strict";
import { EventEmitter, on, once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";

// Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY/local static
// provider, extension callback recorder and private atmd socket proxy.
// These are real callback projections; ctx idle/pending are not wire fields.
const capture = (await readFile(new URL("../crates/atm-pi-adapter/tests/fixtures/pi_turn_events_1.5.2.jsonl", import.meta.url), "utf8"))
	.trim().split("\n").map(JSON.parse);
const cases = [
	["main", "normal", ["input", "agent_start", "context", "turn_end", "agent_settled"]],
	["main", "continuation", ["input", "agent_start", "context", "turn_end", "agent_start", "context", "turn_end", "agent_settled"]],
	["main", "skill", ["input", "agent_start", "context", "turn_end", "agent_settled"]],
	["main", "escape", ["input", "agent_start", "context", "turn_end", "agent_settled"]],
	["main", "compact-success", ["session_before_compact", "session_compact"]],
	["main", "compact-cancel", ["input", "agent_start", "context", "turn_end", "agent_settled", "session_before_compact", "session_compact_failed"]],
	["main", "compact-error", ["session_before_compact", "session_compact_failed"]],
	["auto", "threshold-success", ["input", "agent_start", "context", "turn_end", "session_before_compact", "session_compact", "agent_settled"]],
	["auto", "threshold-cancel", ["input", "agent_start", "context", "turn_end", "session_before_compact", "session_compact_failed", "agent_settled"]],
	["auto", "manual-escape", ["input", "agent_start", "context", "turn_end", "agent_settled", "session_before_compact", "session_compact_failed"]],
	["overflow", "overflow-retry", ["input", "agent_start", "context", "turn_end", "session_before_compact", "session_compact", "agent_start", "context", "turn_end", "agent_settled"]],
	// A queued turn starts while an earlier async failure handler is held.
	// Dropping the live idle guard must expose the stale failure on the wire.
	["late-failure", "queued-failure", ["session_before_compact", "input", "agent_start", "context", "turn_end", "agent_settled"]],
	["main", "new", ["session_shutdown", "session_start"]],
	["main", "reload", ["session_shutdown", "session_start"]],
];

for (const source of ["extensions/pi-atm/extensions/pi-atm.ts", "crates/atm/assets/pi-atm/extensions/pi-atm.ts"]) {
	test(`modern_turn_forwarding: ${source}`, { timeout: 30_000 }, async (t) => {
		const dir = await mkdtemp(join(tmpdir(), "atm-pi-turn-"));
		const server = createServer();
		const sockets = new Set();
		const frames = on(server, "frame", { signal: t.signal });
		const previousEnv = process.env;
		let teardown;
		process.env = { ...previousEnv, ATM_SOCKET: join(dir, "atm.sock"), ATM_DEBUG: "0", TMUX_PANE: "%42" };
		t.after(async () => {
			await teardown?.();
			process.env = previousEnv;
			await frames.return();
			for (const socket of sockets) socket.destroy();
			if (server.listening) await new Promise((resolve) => server.close(resolve));
			await rm(dir, { recursive: true, force: true });
		});
		server.on("connection", (socket) => {
			sockets.add(socket);
			socket.on("close", () => sockets.delete(socket));
			socket.on("error", (error) => server.emit("error", error));
			createInterface({ input: socket }).on("line", (line) => {
				try { server.emit("frame", JSON.parse(line)); }
				catch (error) { server.emit("error", error); }
			});
		});
		server.listen(process.env.ATM_SOCKET);
		await once(server, "listening");
		const code = stripTypeScriptTypes(await readFile(new URL(`../${source}`, import.meta.url), "utf8"));
		const { default: install } = await import(`data:text/javascript;base64,${Buffer.from(`${code}\n// lifecycle ${source}`).toString("base64")}`);
		const handlers = new Map();
		const api = { on: (name, handler) => handlers.set(name, handler) };
		install(api);
		let shutdown = false;
		assert.ok(handlers.has("agent_settled"), "final settlement must be forwarded, not just low-level agent_end");
		assert.ok(handlers.has("session_compact"), "manual success must have an outcome frame");
		assert.ok(handlers.has("session_compact_failed"), "manual failure must have an outcome frame");
		let sessionId;
		let idle;
		const ctx = {
			isIdle: () => idle,
			hasPendingMessages: () => false,
			model: { id: "static", provider: "sealed", contextWindow: 128000 },
			sessionManager: {
				getSessionId: () => sessionId,
				// Synthetic: nonzero usage pins the independent envelope snapshot.
				getEntries: () => [{ type: "message", message: { role: "assistant", usage: { input: 4, output: 1, cost: { total: 0.5 } } } }],
			},
			getContextUsage: () => ({ tokens: 4 }),
		};
		teardown = () => handlers.get("session_shutdown")({}, ctx);
		for (const [run, name, want] of cases) {
			await t.test(`${run}/${name}`, async () => {
				const observed = capture.filter((e) => e.run === run && e.case === name);
				assert.ok(observed.length, "capture case exists");
				for (const event of observed) {
					sessionId = event.session_id;
					idle = event.idle;
					// Synthetic: huge preparation/summary attached to captured outcome
					// fields; none may cross the socket or mutate pi's payload.
					const payload = Object.freeze(event.event.includes("compact")
						? { ...event.payload, preparation: "x".repeat(1_100_000), summary: "private conversation" }
						: { ...event.payload });
					if (event.event === "session_start" && shutdown) {
						handlers.clear();
						install(api);
						shutdown = false;
					}
					await handlers.get(event.event)?.(payload, ctx);
					if (event.event === "session_shutdown") shutdown = true;
					assert.deepEqual(payload.reason, event.payload.reason);
				}
				// A same-stream sentinel proves absence/order without a quiet-time wait.
				await handlers.get("model_select")({ capture_end: true }, ctx);
				const actual = [];
				for (;;) {
					const { value, done } = await frames.next();
					assert.equal(done, false, "forwarder ended before the sentinel");
					const [frame] = value;
					if (frame.type !== "pi_event" || frame.data.event === "atm_idle") continue;
					if (frame.data.event === "model_select" && frame.data.payload.capture_end) break;
					const data = frame.data;
					actual.push(data.event);
					if (data.event.includes("compact")) {
						const original = observed.find((e) => e.event === data.event);
						assert.deepEqual(data.payload, { reason: original.payload.reason });
					}
					if (data.event === "agent_settled") assert.deepEqual(data.payload, {});
					if (["context", "turn_end"].includes(data.event)) {
						assert.deepEqual(data.payload, {});
						assert.deepEqual(data.usage, { cost: 0.5, tokens: 5, contextTokens: 4 });
					}
					assert.ok(Buffer.byteLength(JSON.stringify(frame)) + 1 <= 1_048_576);
				}
				assert.deepEqual(actual, want, "no low-level end may expose idle before final settlement");
			});
		}
	});
}

// Synthetic: held I/O/timers isolate ordering and lifetime, not pi's API.
// The H1 idle/pending window was also measured against real pi 1.5.2.
let harnessId = 0;
async function transportHarness(t, source) {
	const h = { exists: true, sockets: [], timers: [], instances: [] };
	class Socket extends EventEmitter {
		writes = [];
		results = [];
		destroyed = false;
		unref() { this.unrefed = true; return this; }
		write(line) {
			if (this.throwWrite) throw new Error("held write failure");
			this.writes.push(JSON.parse(line));
			return this.results.shift() ?? true;
		}
		destroy() { this.destroyed = true; }
	}
	h.fs = { existsSync: () => h.exists, appendFileSync() {} };
	h.net = { createConnection() { const socket = new Socket(); h.sockets.push(socket); return socket; } };
	h.setTimeout = (fn, delay) => {
		const timer = { fn, delay, cleared: false, unrefed: false, unref() { this.unrefed = true; return this; } };
		h.timers.push(timer);
		return timer;
	};
	h.clearTimeout = (timer) => { timer.cleared = true; };
	h.pendingTimers = (delay) => h.timers.filter((timer) => !timer.cleared && timer.delay === delay);
	h.release = (delay) => {
		const timer = h.pendingTimers(delay)[0];
		assert.ok(timer, `expected a controlled ${delay}ms timer`);
		timer.cleared = true;
		timer.fn();
	};
	const key = `__atmLifecycleHarness${++harnessId}`;
	globalThis[key] = h;
	const code = stripTypeScriptTypes(await readFile(new URL(`../${source}`, import.meta.url), "utf8"))
		.replace('import * as fs from "node:fs";', `const fs = globalThis.${key}.fs;`)
		.replace('import * as net from "node:net";', `const net = globalThis.${key}.net;`);
	const prelude = `const {setTimeout, clearTimeout} = globalThis.${key};\n`;
	const { default: install } = await import(`data:text/javascript;base64,${Buffer.from(prelude + code).toString("base64")}`);
	h.instance = (ctx = {}) => {
		const state = { id: "real-session", idle: true, pending: false, model: { id: "probe", provider: "synthetic", contextWindow: 8192 }, ...ctx };
		const context = {
			isIdle: () => state.idle,
			hasPendingMessages: () => state.pending,
			get model() { return state.model; },
			sessionManager: { getSessionId: () => state.id },
			ui: state.ui,
		};
		const handlers = new Map();
		install({ on: (name, handler) => handlers.set(name, handler) });
		const instance = { state, ctx: context, handlers, emit: (name, payload = {}) => handlers.get(name)?.(payload, context) };
		h.instances.push(instance);
		return instance;
	};
	h.events = (socket) => socket.writes.filter((frame) => frame.type === "pi_event").map((frame) => frame.data);
	h.idles = (socket) => h.events(socket).filter((frame) => frame.event === "atm_idle");
	h.reconnect = () => {
		const socket = h.sockets.at(-1);
		socket.emit("close");
		h.release(1000);
		assert.notEqual(h.sockets.at(-1), socket, "retry must open without another pi event");
		return h.sockets.at(-1);
	};
	t.after(async () => {
		for (const instance of h.instances) await instance.emit("session_shutdown");
		delete globalThis[key];
	});
	return h;
}

for (const source of ["extensions/pi-atm/extensions/pi-atm.ts", "crates/atm/assets/pi-atm/extensions/pi-atm.ts"]) {
	test(`idle_reconnect_registers_without_input: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		const i = h.instance();
		await i.emit("session_start");
		const first = h.sockets[0];
		first.emit("connect");
		assert.deepEqual(first.writes.map((f) => f.type === "connect" ? "connect" : f.data.event), ["connect", "session_start", "atm_idle"]);
		assert.equal(h.idles(first)[0].usage, undefined);
		const second = h.reconnect();
		second.emit("connect");
		assert.deepEqual(second.writes.map((f) => f.type === "connect" ? "connect" : f.data.event), ["connect", "atm_idle"]);
		assert.equal(h.idles(second)[0].session_id, "real-session");
		assert.equal(h.idles(second)[0].pid, process.pid);
		assert.deepEqual(h.idles(second)[0].model, i.state.model);
		second.emit("drain");
		assert.equal(h.idles(second).length, 1, "not a heartbeat or drain duplicate");
	});
	test(`missing/refused/hung connections retry once: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		h.exists = false;
		const i = h.instance();
		await i.emit("session_start");
		assert.equal(h.pendingTimers(1000).length, 1, "missing path must schedule retry");
		assert.ok(h.pendingTimers(1000)[0].unrefed);
		h.release(1000);
		assert.equal(h.pendingTimers(1000).length, 1);
		h.exists = true;
		h.release(1000);
		const refused = h.sockets.at(-1);
		refused.emit("error", new Error("refused"));
		refused.emit("close");
		assert.equal(h.pendingTimers(1000).length, 1);
		h.release(1000);
		const hanging = h.sockets.at(-1);
		assert.ok(h.pendingTimers(10000)[0].unrefed);
		await i.emit("model_select");
		assert.equal(h.sockets.length, 2, "one pending attempt");
		h.release(10000);
		assert.ok(hanging.destroyed, "only a pending connect may time out");
		h.release(1000);
		const connected = h.sockets.at(-1);
		connected.emit("connect");
		assert.equal(h.pendingTimers(10000).length, 0, "established idle socket has no guard");
	});
	test(`idle_snapshot_waits_for_every_drain_and_reads_current_context: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		const i = h.instance();
		await i.emit("session_start");
		const socket = h.sockets[0];
		let reads = 0;
		const isIdle = i.ctx.isIdle;
		i.ctx.isIdle = () => { reads++; return isIdle(); };
		socket.results = [false, true, false];
		socket.emit("connect");
		assert.equal(socket.writes.length, 1, "handshake backpressure must pause FIFO");
		await i.emit("model_select", { queued: true });
		i.state.id = "current-session";
		i.state.model = { id: "current-model", provider: "synthetic", contextWindow: 4096 };
		socket.emit("drain");
		assert.equal(h.idles(socket).length, 0, "last queued write=false is not a completed flush");
		assert.equal(reads, 0, "do not sample context before the final drain");
		socket.emit("drain");
		assert.equal(h.idles(socket).length, 1);
		assert.equal(h.idles(socket)[0].session_id, "current-session");
		assert.deepEqual(h.idles(socket)[0].model, i.state.model);
		socket.emit("drain");
		assert.equal(h.idles(socket).length, 1);
	});
	test(`busy/pending/native/select/missing/stale vetoes: ${source}`, async (t) => {
		for (const veto of ["busy", "pending", "native", "select", "missing", "stale", "pending-id"]) {
			await t.test(veto, async (t) => {
				const h = await transportHarness(t, source);
				const selection = Promise.withResolvers();
				const i = h.instance({ ui: { select: () => selection.promise } });
				await i.emit("session_start");
				if (veto === "busy") i.state.idle = false;
				if (veto === "pending") i.state.pending = true;
				if (veto === "native") {
					assert.ok(i.handlers.has("ui_prompt_start"), "observe native UI locally");
					await i.emit("ui_prompt_start");
				}
				const selecting = veto === "select" ? i.ctx.ui.select("held", ["yes"]) : undefined;
				const idleGetter = i.ctx.isIdle;
				if (veto === "missing") delete i.ctx.isIdle;
				if (veto === "stale") i.ctx.isIdle = () => { throw new Error("stale ctx"); };
				if (veto === "pending-id") i.state.id = "pending-123";
				const socket = h.sockets[0];
				assert.doesNotThrow(() => socket.emit("connect"));
				assert.equal(h.idles(socket).length, 0);
				i.state.idle = true; i.state.pending = false; i.state.id = "current-session"; i.ctx.isIdle = idleGetter;
				if (veto === "native") await i.emit("ui_prompt_end");
				selection.resolve("yes");
				await selecting;
				socket.emit("drain");
				assert.equal(h.idles(socket).length, 0, "failed predicate is consumed, not deferred heartbeat");
				const next = h.reconnect();
				next.emit("connect");
				assert.equal(h.idles(next).length, 1, "positive later reconnect twin");
			});
		}
	});
	test(`H1_held_input_has_one_idle_edge: ${source}`, async (t) => {
		for (const includeReconnect of [false, true]) {
			await t.test(String(includeReconnect), async (t) => {
				const h = await transportHarness(t, source);
				const i = h.instance();
				await i.emit("session_start");
				await i.emit("input", Object.freeze({ source: "interactive", text: "neutral start line" }));
				const socket = h.sockets[0];
				// Hold the actual-source connect callback until the input callback
				// has returned but the later pi input handler is still pending.
				assert.equal(i.ctx.isIdle(), true);
				assert.equal(i.ctx.hasPendingMessages(), false);
				socket.emit("connect");
				if (includeReconnect) {
					const next = h.reconnect();
					next.emit("connect");
				}
				i.state.idle = false;
				await i.emit("agent_start");
				i.state.idle = true;
				await i.emit("agent_settled");
				let before = "idle", edges = 0;
				for (const frame of h.sockets.flatMap(h.events)) {
					const state = ["input", "agent_start"].includes(frame.event) ? "working"
						: ["agent_settled", "atm_idle", "session_start"].includes(frame.event) ? "idle" : before;
					if (state === "idle" && before !== "idle") edges++;
					before = state;
				}
				assert.equal(edges, 1, "one input/run must not invent a second handover turn end");
			});
		}
	});
	test(`handled input withholds recovery until genuine boundary: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		const i = h.instance();
		await i.emit("session_start");
		h.sockets[0].emit("connect");
		await i.emit("input", { source: "interactive", text: "consumed without a run" });
		const held = h.reconnect();
		held.emit("connect");
		assert.equal(h.idles(held).length, 0, "idle=true cannot clear an input latch");
		await i.emit("model_select");
		held.emit("drain");
		assert.equal(h.idles(held).length, 0, "unrelated activity cannot clear input latch");
		i.state.idle = false;
		await i.emit("agent_start");
		i.state.idle = true;
		await i.emit("agent_settled");
		const recovered = h.reconnect();
		recovered.emit("connect");
		assert.equal(h.idles(recovered).length, 1);
	});
	test(`shutdown cancels old callbacks and same-module factories stay isolated: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		const selection = Promise.withResolvers();
		const ui = { select: () => selection.promise };
		const old = h.instance({ id: "old-session", ui });
		await old.emit("session_start");
		const socket = h.sockets[0];
		socket.emit("connect");
		const selecting = ui.select("old select", ["yes"]);
		await old.emit("session_shutdown");
		assert.ok(socket.destroyed);
		let staleReads = 0;
		old.ctx.isIdle = () => { staleReads++; throw new Error("stale"); };
		old.ctx.sessionManager.getSessionId = () => { staleReads++; throw new Error("stale"); };
		await old.emit("model_select");
		const current = h.instance({ id: "new-session", ui });
		await current.emit("session_start");
		const next = h.sockets.at(-1);
		next.emit("connect");
		const before = next.writes.length;
		for (const event of ["connect", "drain", "close"]) socket.emit(event);
		socket.emit("error", new Error("late"));
		for (const timer of [...h.timers]) if (timer.cleared) timer.fn();
		selection.resolve("yes");
		assert.equal(await selecting, "yes");
		assert.equal(staleReads, 0);
		assert.equal(next.writes.length, before, "late old select must not emit through new owner");
		assert.equal(h.idles(next)[0].session_id, "new-session");
		await current.emit("session_shutdown");
		for (const timer of h.timers) timer.fn();
		assert.equal(h.sockets.length, 2, "shutdown cannot reopen an old or new instance");
	});
	test(`pending/retry shutdown cannot reopen: ${source}`, async (t) => {
		for (const pending of [false, true]) {
			const h = await transportHarness(t, source);
			const i = h.instance();
			await i.emit("session_start");
			const socket = h.sockets[0];
			if (!pending) socket.emit("close");
			await i.emit("session_shutdown");
			assert.equal(h.pendingTimers(1000).length + h.pendingTimers(10000).length, 0);
			for (const timer of h.timers) timer.fn();
			socket.emit("connect"); socket.emit("drain"); socket.emit("close");
			assert.equal(h.sockets.length, 1);
			assert.equal(socket.writes.length, 0, "disconnected shutdown never opens or queues terminal traffic");
		}
	});
	test(`idle_reconnect_real_receiver_without_input: ${source}`, { timeout: 30_000 }, async (t) => {
		const dir = await mkdtemp(join(tmpdir(), "atm-pi-retry-"));
		const server = createServer();
		const sockets = new Set();
		const frames = on(server, "frame", { signal: t.signal });
		const timers = new EventEmitter();
		const retries = on(timers, "retry", { signal: t.signal });
		const previousEnv = process.env;
		const key = `__atmLifecycleHarness${++harnessId}`;
		const clock = {
			attempts: 0,
			setTimeout(fn, delay) {
				const timer = { fn, cleared: false, unref() { return this; } };
				if (delay === 1000) timers.emit("retry", timer);
				return timer;
			},
			clearTimeout(timer) { timer.cleared = true; },
		};
		globalThis[key] = clock;
		process.env = { ...previousEnv, ATM_SOCKET: join(dir, "atm.sock"), ATM_DEBUG: "0", TMUX_PANE: "%42" };
		let shutdown;
		t.after(async () => {
			await shutdown?.();
			process.env = previousEnv;
			for (const socket of sockets) socket.destroy();
			await frames.return(); await retries.return();
			if (server.listening) await new Promise((resolve) => server.close(resolve));
			delete globalThis[key];
			await rm(dir, { recursive: true, force: true });
		});
		let connections = 0;
		server.on("connection", (socket) => {
			connections++;
			sockets.add(socket);
			socket.on("close", () => sockets.delete(socket));
			socket.on("error", () => {});
			createInterface({ input: socket }).on("line", (line) => server.emit("frame", JSON.parse(line)));
		});
		server.listen(process.env.ATM_SOCKET);
		await once(server, "listening");
		const code = stripTypeScriptTypes(await readFile(new URL(`../${source}`, import.meta.url), "utf8"))
			.replace('import * as net from "node:net";', 'import * as realNet from "node:net";');
		const prelude = `const {setTimeout,clearTimeout}=globalThis.${key};
			const net={createConnection(...args){globalThis.${key}.attempts++;return realNet.createConnection(...args);}};\n`;
		const { default: install } = await import(`data:text/javascript;base64,${Buffer.from(prelude + code).toString("base64")}`);
		const handlers = new Map();
		install({ on: (name, handler) => handlers.set(name, handler) });
		const ctx = { isIdle: () => true, hasPendingMessages: () => false,
			sessionManager: { getSessionId: () => "receiver-session" },
			model: { id: "probe", provider: "synthetic", contextWindow: 8192 } };
		shutdown = () => handlers.get("session_shutdown")({}, ctx);
		await handlers.get("session_start")({}, ctx);
		async function connectionFrames() {
			const received = [];
			for (;;) {
				const { value, done } = await frames.next();
				assert.equal(done, false);
				const [frame] = value;
				received.push(frame.type === "connect" ? "connect" : frame.data.event);
				if (frame.data?.event === "atm_idle") {
					assert.equal(frame.data.session_id, "receiver-session");
					assert.equal(frame.data.tmux_pane, "%42");
					assert.equal(frame.data.usage, undefined);
					return received;
				}
			}
		}
		assert.deepEqual(await connectionFrames(), ["connect", "session_start", "atm_idle"]);
		for (const socket of sockets) socket.destroy();
		const { value: [retry] } = await retries.next();
		assert.equal(connections, 1);
		retry.fn(); // Explicitly release backoff; no new pi callback or sleep.
		assert.equal(clock.attempts, 2, "retry must initiate a second real connection without input");
		assert.deepEqual(await connectionFrames(), ["connect", "atm_idle"]);
		assert.equal(connections, 2);
	});
	test(`dialog_idle_resolution_completion: ${source}`, async (t) => {
		// Missing closure sampling, opening-time snapshots and skipped error finally
		// must fail wire assertions; dialog values/errors belong to pi unchanged.
		for (const [name, opening, closing, mode, want] of [
			["pick", true, true, "resolve", { idle: true }],
			["cancel", true, true, "cancel", { idle: true }],
			["reject", false, true, "reject", { idle: true }],
			["busy-at-close", true, false, "resolve", { idle: false }],
			["idle-at-close", false, true, "resolve", { idle: true }],
			["missing", true, true, "missing", {}],
			["nonboolean", true, true, "nonboolean", {}],
			["stale", true, true, "stale", undefined],
			["stale-reject", true, true, "stale-reject", undefined],
			["throw", true, true, "throw", { idle: true }],
		]) await t.test(name, async (t) => {
			const h = await transportHarness(t, source);
			const selection = Promise.withResolvers();
			const error = new Error("original select failure");
			const options = Object.freeze(["Alpha", "Beta"]), opts = Object.freeze({ marker: true });
			const ui = { select(title, actualOptions, actualOpts) {
				assert.equal(this, ui);
				assert.equal(title, "Neutral selection");
				assert.equal(actualOptions, options); assert.equal(actualOpts, opts);
				if (mode === "throw") throw error;
				return selection.promise;
			} };
			const i = h.instance({ ui, idle: opening });
			await i.emit("session_start");
			const socket = h.sockets[0]; socket.emit("connect");
			const dialogs = () => h.events(socket).filter((f) => f.event.startsWith("atm_needs_input_"));
			const outcome = ui.select("Neutral selection", options, opts).then(
				(value) => ({ value }), (reason) => ({ error: reason }));
			assert.deepEqual(dialogs().slice(0, 1).map((f) => [f.event, f.payload]), [["atm_needs_input_open", { title: "Neutral selection" }]]);
			if (mode !== "throw") assert.equal(dialogs().length, 1, "no resolution before original select settles");
			i.state.idle = closing;
			if (mode === "missing") delete i.ctx.isIdle;
			if (mode === "nonboolean") i.ctx.isIdle = () => "true";
			if (mode === "stale" || mode === "stale-reject") i.ctx.isIdle = () => { throw new Error("inactive context"); };
			const rejects = ["reject", "stale-reject", "throw"].includes(mode);
			if (mode.endsWith("reject")) selection.reject(error);
			else selection.resolve(mode === "cancel" ? undefined : "Alpha");
			const actual = await outcome;
			assert.deepEqual(actual, rejects ? { error } : { value: mode === "cancel" ? undefined : "Alpha" });
			if (rejects) assert.equal(actual.error, error, "reporting must preserve the exact original error");
			assert.deepEqual(dialogs().slice(1).map((f) => [f.event, f.payload]),
				want === undefined ? [] : [["atm_needs_input_resolved", want]]);
			assert.deepEqual(options, ["Alpha", "Beta"]); assert.deepEqual(opts, { marker: true });
			// A throwing getter cannot leak the span and veto a later reconnect.
			i.ctx.isIdle = () => true;
			const next = h.reconnect(); next.emit("connect");
			assert.equal(h.idles(next).length, 1);
		});
	});
	test(`dialog_idle_resolution_input_latch: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		let selection = Promise.withResolvers();
		const i = h.instance({ ui: { select: () => selection.promise } });
		await i.emit("session_start");
		let socket = h.sockets[0]; socket.emit("connect");
		const payload = Object.freeze({ source: "interactive", text: "neutral pending input" });
		await i.emit("input", payload);
		const selecting = i.ctx.ui.select("Latched selection", ["Alpha"]);
		assert.equal(i.ctx.isIdle(), true); assert.equal(i.ctx.hasPendingMessages(), false);
		await i.emit("model_select"); await i.emit("ui_prompt_start"); await i.emit("ui_prompt_end");
		selection.resolve("Alpha"); assert.equal(await selecting, "Alpha");
		const resolved = h.events(socket).filter((f) => f.event === "atm_needs_input_resolved");
		assert.deepEqual(resolved.map((f) => f.payload), [{}], "input latch vetoes closure idle, not just reconnect idle");
		assert.deepEqual(h.events(socket).find((f) => f.event === "input").payload, payload);
		assert.equal(Object.hasOwn(payload, "idle"), false);
		socket = h.reconnect(); socket.emit("connect");
		assert.equal(h.idles(socket).length, 0, "dialog closure must not clear input latch");
		i.state.idle = false; await i.emit("agent_start");
		i.state.idle = true; await i.emit("agent_settled");
		selection = Promise.withResolvers();
		const recovered = i.ctx.ui.select("Recovered selection", ["Alpha"]);
		selection.resolve("Alpha"); assert.equal(await recovered, "Alpha");
		assert.deepEqual(h.events(socket).filter((f) => f.event === "atm_needs_input_resolved").map((f) => f.payload), [{ idle: true }]);
		const next = h.reconnect(); next.emit("connect");
		assert.equal(h.idles(next).length, 1, "genuine settlement permits recovery");
	});
	test(`dialog_idle_resolution_overlap: ${source}`, async (t) => {
		// Resolving every inner close would clear attention while a sibling waits.
		for (const [first, mode] of [[0, "resolve"], [1, "cancel"], [0, "reject"], [1, "reject"], [1, "throw"]]) {
			await t.test(`${first}/${mode}`, async (t) => {
				const h = await transportHarness(t, source);
				const pending = [Promise.withResolvers(), Promise.withResolvers()];
				const error = new Error("inner select failure");
				let calls = 0;
				const i = h.instance({ ui: { select() {
					const call = calls++;
					if (mode === "throw" && call === 1) throw error;
					return pending[call]?.promise ?? Promise.resolve("Alpha");
				} } });
				await i.emit("session_start");
				let socket = h.sockets[0]; socket.emit("connect");
				const all = () => h.sockets.flatMap(h.events).filter((f) => f.event.startsWith("atm_needs_input_"));
				const results = ["First", "Second"].map((title) => i.ctx.ui.select(title, ["Alpha"]).then(
					(value) => ({ value }), (reason) => ({ error: reason })));
				assert.deepEqual(all().map((f) => [f.event, f.payload]), [["atm_needs_input_open", { title: "First" }]]);
				if (mode === "reject") pending[first].reject(error);
				else pending[first].resolve(mode === "cancel" ? undefined : "Alpha");
				const actual = await results[first];
				assert.deepEqual(actual, mode === "reject" || mode === "throw" ? { error } : { value: mode === "cancel" ? undefined : "Alpha" });
				if (mode === "reject" || mode === "throw") assert.equal(actual.error, error);
				assert.equal(all().length, 1, "no resolved frame until the final sibling closes");
				socket = h.reconnect(); socket.emit("connect");
				assert.equal(h.idles(socket).length, 0, "remaining sibling vetoes reconnect idle");
				pending[1 - first].resolve("Alpha"); await results[1 - first];
				assert.deepEqual(all().map((f) => [f.event, f.payload]), [
					["atm_needs_input_open", { title: "First" }], ["atm_needs_input_resolved", { idle: true }],
				]);
				socket.emit("drain"); assert.equal(h.idles(socket).length, 0, "consumed snapshot is not a heartbeat");
				assert.equal(await i.ctx.ui.select("Next", ["Alpha"]), "Alpha");
				assert.deepEqual(all().slice(2).map((f) => [f.event, f.payload]), [
					["atm_needs_input_open", { title: "Next" }], ["atm_needs_input_resolved", { idle: true }],
				]);
			});
		}
	});
	test(`bounded outage FIFO and write failures: ${source}`, async (t) => {
		const h = await transportHarness(t, source);
		const i = h.instance();
		await i.emit("session_start");
		for (let seq = 0; seq < 260; seq++) await i.emit("model_select", { seq });
		const socket = h.sockets[0];
		socket.emit("connect");
		const queued = h.events(socket).filter((frame) => frame.event === "model_select");
		assert.equal(queued.length, 256);
		assert.equal(queued[0].payload.seq, 4);
		assert.equal(queued.at(-1).payload.seq, 259);
		assert.equal(h.idles(socket).length, 1);
		socket.throwWrite = true;
		await i.emit("model_select", { seq: 260 });
		assert.ok(socket.destroyed);
		h.release(1000);
		const next = h.sockets.at(-1);
		next.emit("connect");
		assert.equal(h.events(next)[0].payload.seq, 260, "a synchronous failed write remains queued");
	});
}
