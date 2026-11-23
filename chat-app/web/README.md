# ATM chat web app

This is the phone/desktop channel view. It uses the ATM 1.1 JSON envelope over
same-origin `/ws`. With W0 and the channel daemon, **Needs you** shows open
decisions even without a linked conversation. The fake and its WebSocket
server never go into the distribution.

Direct messages show replies inline with a short quote of the thread root;
use the main composer to send an ordinary message. The reply count or the
revealed reply action still opens a focused thread. Project conversations
keep their separate threads.

Use browser or Android Back to leave a thread or conversation one view at a
time; Back at Home leaves the app. Returning to a view keeps its scroll
position and draft. Pull down from the top of the conversation list or
channel history on touch to fetch fresh data without reloading. An offline
pull leaves the cached content and Offline banner in place.

**Known limits for direct messages:**

- If a thread root is older than the loaded history, its replies only appear
  after you load that root (use **Load older posts**). An unseen reply to that
  root can be marked read if a newer visible post advances the channel's
  seen marker past it.
- A regular reload starts at Home, even offline. Select the cached
  conversation to read it. If a thread was open when the display was saved,
  only that thread's replies may be cached; other inline replies return on
  reconnect.

## Build and try it locally

Use Node 24 and the project's sealed environment (private HOME/npm cache,
ATM socket and tmux directory; no inherited TMUX, TMUX_PANE or installed ATM
binaries on PATH). From this directory:

```sh
npm ci --include=dev --ignore-scripts --no-audit --no-fund
npm run dev
```

Open the printed loopback URL. The dev server speaks to a synthetic data
store only; refreshing the dev server loses fixture changes. No service
worker is registered in dev mode. `ws` 8.x is used only in that server and
the tests because Node's native WebSocket API is a client.

```sh
npm run build
npm test
npm run size
```

Serve `dist/` at the origin root with the ATM Chat bridge (`--page-dir` in
its README). Restart the bridge when updating this directory; it snapshots
the files on startup. Use the existing authorized HTTPS/Tailscale origin for
installation from the browser menu. When an update is ready, use **Reload**;
settle uncertain file sends or decision answers before reloading. This resumes
view, drafts and available scroll bookmarks. The service worker caches one
versioned static shell.

Text sends enter a local queue before leaving the composer. The visible tab
holding the queue sends them in order per conversation when connected; a
refused entry can be retried or deleted without blocking subsequent posts.
Photos and files still use the online composer and manual retry outside the
text queue; a file can arrive before a waiting text. Home lists all unsent
rows marked **Not sent** and their destinations for local copy or deletion.
The queue and separate display snapshot are kept unencrypted in this browser profile. Clear
site data to remove them (including any unsent messages).

For a local smoke, install `playwright@1.58.2` and its matching Chromium
headless shell into private scratch under the seal, not this package. Set
`ATM_CHAT_TEST_PLAYWRIGHT` to the absolute path of its `index.mjs` and
`PLAYWRIGHT_BROWSERS_PATH` to the private download directory. With `dist/`
built, run `node test/local/smoke.mjs --chat` and
`node test/local/smoke.mjs --pwa`. Both serve synthetic data on an ephemeral
loopback port. With the same sealed tooling and `dist/`, run
`npm test -- --config test/local/vitest.config.mjs` for the Chromium keyboard
focus regression. These checks cover responsive widths and offline/update
behavior in Chromium, not the physical Fold or the live tailnet.

### Real-stack smoke (Linux)

Build `atmd`, `atm` and `atm-chat-bridge` from the checkout, and build `dist/`.
With the same Playwright and Chromium environment above pointing to existing
local caches, run from this directory:

```sh
test/local/seal-real.sh "$ATMD_BIN" "$ATM_BIN" "$CHAT_BRIDGE_BIN"
```

Pass absolute checkout-built binary paths. The wrapper runs `smoke.mjs --real`
with a private HOME, socket, tmux server, PID and loopback network namespace.
It needs Linux user namespaces, `unshare`, `ip`, `tmux`, OpenSSL and Node 24.
It copies the local browser/tooling into the seal; it never downloads them.
Its test-only loopback TLS hop stands in for Tailscale Serve: it sets the
fixture login on every request, overwrites any client-supplied login, and
never changes Origin. The private Chromium trusts only the fixture key for
its self-signed certificate; service-worker startup is verified. No tailscale
command runs. Set `ATM_CHAT_TEST_SHOTS` to a directory to keep the three PNGs.
The smoke checks actual decision links when the daemon supplies them and
still exercises unlinked cards against older daemons. It does not test live
Tailnet or physical device behavior.
