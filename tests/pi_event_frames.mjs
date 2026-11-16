import assert from "node:assert/strict";
import { on, once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";

const sources = [
	"extensions/pi-atm/extensions/pi-atm.ts",
	"crates/atm/assets/pi-atm/extensions/pi-atm.ts",
];
const model = { id: "probe", provider: "synthetic", contextWindow: 8192 };
const maxFrameBytes = 1_048_576; // atmd's inclusive input-line limit, including newline.

for (const source of sources) {
	test(source, { timeout: 30_000 }, async (t) => {
		const dir = await mkdtemp(join(tmpdir(), "atm-pi-"));
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
				try {
					server.emit("frame", { frame: JSON.parse(line), bytes: Buffer.byteLength(line) + 1 });
				} catch (error) {
					server.emit("error", error);
				}
			});
		});
		server.listen(process.env.ATM_SOCKET);
		await once(server, "listening");

		// Strip the real extension, not a copied projection helper. A unique URL
		// gives source and install mirror their own module-scoped connection.
		const code = stripTypeScriptTypes(await readFile(new URL(`../${source}`, import.meta.url), "utf8"));
		const { default: install } = await import(`data:text/javascript;base64,${Buffer.from(`${code}\n// ${source}`).toString("base64")}`);
		const handlers = new Map();
		install({ on: (name, handler) => handlers.set(name, handler) });
		const ctx = {
			model: { ...model, headers: { "X-Probe": "must-not-be-forwarded" } },
			sessionManager: {
				getSessionId: () => "frame-session",
				getEntries: () => [
					{ type: "message", message: { role: "assistant", usage: { input: 10, output: 20, cacheRead: 30, cacheWrite: 40, cost: { total: 0.25 } } } },
					{ type: "message", message: { role: "assistant", usage: { input: 4, output: 3, cacheRead: 2, cacheWrite: 1, cost: { total: 0.5 } } } },
				],
			},
			getContextUsage: () => ({ tokens: 4567 }),
		};
		async function emit(name, payload, context = ctx) {
			await handlers.get(name)(payload, context);
			for (;;) {
				const { value, done } = await frames.next();
				assert.equal(done, false, "forwarder ended before the event frame");
				const [{ frame, bytes }] = value;
				if (frame.type === "pi_event") {
					assert.deepEqual(frame.protocol_version, { major: 1, minor: 0 });
					assert.equal(frame.data.event, name);
					assert.equal(frame.data.session_id, "frame-session");
					assert.equal(frame.data.pid, process.pid);
					assert.equal(frame.data.tmux_pane, "%42");
					assert.deepEqual(frame.data.model, model);
					return { data: frame.data, bytes };
				}
			}
		}

		// Synthetic: conversation bodies larger than the daemon limit. These
		// must never displace the usage snapshot or prevent the terminal frame.
		const message = Object.freeze({ role: "assistant", content: Object.freeze([{ type: "text", text: "x".repeat(1_100_000) }]) });
		for (const name of ["context", "turn_end", "agent_end"]) {
			await t.test(`${name} omits its conversation body`, async () => {
				const payload = Object.freeze(name === "turn_end"
					? { type: name, turnIndex: 0, message, toolResults: [] }
					: { type: name, messages: [message] });
				const { data, bytes } = await emit(name, payload);
				assert.ok(bytes <= maxFrameBytes, `${name} frame exceeds atmd's line limit`);
				assert.equal(Object.getPrototypeOf(data.payload), Object.prototype);
				assert.deepEqual(Object.keys(data.payload), [], `${name} must not forward conversation content`);
				assert.deepEqual(data.usage, name === "agent_end" ? undefined : { cost: 0.75, tokens: 110, contextTokens: 4567 });
				assert.equal(message.content[0].text.length, 1_100_000, "pi's message was mutated");
			});
		}
		await t.test("missing usage never falls back to the context body", async () => {
			const { data } = await emit("context", { messages: [{ role: "assistant", content: [{ type: "text", text: "not a usage snapshot" }] }] }, {
				...ctx,
				sessionManager: { ...ctx.sessionManager, getEntries() { throw new Error("session unavailable"); } },
			});
			assert.deepEqual(data.payload, {});
			assert.deepEqual(data.usage, {});
		});
		await t.test("other event payloads remain intact", async () => {
			const payload = { type: "tool_call", toolName: "read", toolCallId: "call-probe", input: { path: "note.txt" } };
			const { data } = await emit("tool_call", payload);
			assert.deepEqual(data.payload, payload);
			assert.equal(data.usage, undefined);
		});
	});
}
