# ATM Phone Chat

A tailnet-only bridge for reading and replying to ATM messages. The bridge
includes a placeholder page until the responsive web client is built. That
client is one page for the phone cover screen, unfolded screen, and desktop.

## Build and run

On a Unix host with Rust 1.88 or newer:

```sh
cargo build --locked --manifest-path chat-app/bridge/Cargo.toml
chat-app/bridge/target/debug/atm-chat-bridge \
  --socket "$ATMD_SOCKET" --owner-login "$TAILSCALE_LOGIN" \
  --public-origin "$TAILNET_HTTPS_ORIGIN"
```

Set `ATMD_SOCKET` to the daemon's Unix socket path. Set `TAILSCALE_LOGIN` to
its operator's Tailscale login, and `TAILNET_HTTPS_ORIGIN` to the tailnet HTTPS
origin (lowercase host, no trailing slash; omit the port for 443). The login
flag is visible locally in the process list. The loopback port defaults to
8765; change it with `--port`. Run the foreground `tailscale serve` command
printed by the bridge, then open the printed tailnet URL. Do not use Funnel.

When the web client is built, pass `--page-dir chat-app/web/dist` from the repo
root, or place `web/dist` beside the bridge executable. Without that directory
the bridge serves its placeholder. Restart it after changing the static build;
assets are loaded at startup.

## Boundaries

The bridge listens only on `127.0.0.1`. It trusts the identity header only
because the intended remote entry point is Tailscale Serve. Local processes
can supply headers directly; don't put another proxy in front or run this on
a host with untrusted local users. Each request requires the configured login;
a WebSocket upgrade also requires the configured HTTPS `Origin`.

Forwarded calls run with owner scope. The bridge permits `channel.list`,
`channel.history`, `channel.post`, `channel.seen`, `decision.list`,
`decision.resolve`, `decision.withdraw`, `org.tree`, `agent.list`,
`channel.edit`, `channel.remove`, and `channel.react`. Watch accepts exactly
`["channel.", "decision.", "msg."]` in that order. Other RPCs, including the
legacy `msg.*` methods and pane controls, are not available through the
bridge. The native CLI is unchanged.

These routes exist only when the bridge is started with the flag shown:

- `POST /upload?name=<file name>` (`--files-dir`) stores one file of up to
  32 MiB in the files directory; it also requires the `Origin`.
- `GET /file?p=<absolute path>` (`--files-dir`) returns one file of up to
  32 MiB from the files directory or a `--serve-root`; below that directory,
  a path component that starts with a dot is refused.
- `/notify/subscription` and `/notify/latest` (`--notify-dir`) read and
  change the stored push subscription and return the latest notification;
  changing the subscription also requires the `Origin`.

The WebSocket uses one text message per daemon JSON record without its
terminating newline. The web client should send an ATM protocol `ping` about
every 60 seconds during inactivity: the bridge has a 300-second read-idle
limit on each side. Reconnect and catch up after a disconnect.

`GET /health` requires the configured login. It returns HTTP 200 and uncached
JSON: `{"bridge":"ok","daemon":"up","protocol":"1.1"}` (or `"daemon":"down"`).
`up` means the configured Unix socket accepted a connection, not that a
protocol session or any RPC is ready. A refused identity returns HTTP 403
`{"error":"identity"}` without dialing. On `/ws`, a valid login with a
refused Origin returns HTTP 403 `{"error":"origin"}` even without an upgrade.

After an upgrade, the bridge attempts these WebSocket closes:

| Condition | Code | Reason |
|---|---:|---|
| Cannot dial the Unix socket | 1011 | `daemon unavailable` |
| Unix connection fails after dialing | 1011 | `daemon disconnected` |
| No client `Connect` within 10 seconds of a successful daemon dial | 1001 | `idle timeout` |
| No first daemon reply within 10 seconds after forwarding `Connect` | 1001 | `idle timeout` |
| Either side is read-idle for 300 seconds in steady state | 1001 | `idle timeout` |

A daemon that accepts the connection but stays silent after `Connect` times
out; that is not a disconnect. A broken transport may not deliver a Close
frame. Probe `/health` to check whether a reachable bridge can still reach
atmd before reconnecting.

## Check

Run `chat-app/check.sh` for formatting, linting, and tests against a sealed
checkout-built `atmd`. From a separate checkout of `chat-app`, set
`ATM_CHAT_TEST_ATMD` to the absolute path of a compatible `atmd` executable
and run `./check.sh`. No running daemon or Tailscale configuration is needed
for the tests.
