import assert from "node:assert/strict";
import { on, once } from "node:events";
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
		process.env = { ...previousEnv, ATM_SOCKET: join(dir, "atm.sock"), ATM_DEBUG: "0", TMUX_PANE: "%42" };
		t.after(async () => {
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
		install({ on: (name, handler) => handlers.set(name, handler) });
		assert.ok(handlers.has("agent_settled"), "final settlement must be forwarded, not just low-level agent_end");
		assert.ok(handlers.has("session_compact"), "manual success must have an outcome frame");
		assert.ok(handlers.has("session_compact_failed"), "manual failure must have an outcome frame");
		let sessionId;
		let idle;
		const ctx = {
			isIdle: () => idle,
			model: { id: "static", provider: "sealed", contextWindow: 128000 },
			sessionManager: {
				getSessionId: () => sessionId,
				// Synthetic: nonzero usage pins the independent envelope snapshot.
				getEntries: () => [{ type: "message", message: { role: "assistant", usage: { input: 4, output: 1, cost: { total: 0.5 } } } }],
			},
			getContextUsage: () => ({ tokens: 4 }),
		};
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
					await handlers.get(event.event)?.(payload, ctx);
					assert.deepEqual(payload.reason, event.payload.reason);
				}
				// A same-stream sentinel proves absence/order without a quiet-time wait.
				await handlers.get("model_select")({ capture_end: true }, ctx);
				const actual = [];
				for (;;) {
					const { value, done } = await frames.next();
					assert.equal(done, false, "forwarder ended before the sentinel");
					const [frame] = value;
					if (frame.type !== "pi_event") continue;
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
