#!/usr/bin/env bash
# Linux-only. Fail closed if any part of the isolation cannot be set up.
set -euo pipefail
usage='usage: test/local/seal-real.sh /absolute/atmd /absolute/atm /absolute/atm-chat-bridge'
[[ $# == 3 ]] || { echo "$usage" >&2; exit 2; }
for path in "$@"; do
  [[ $path == /* && -f $path && -x $path ]] || { echo "$usage" >&2; exit 2; }
  case $(realpath -- "$path") in
    "$(realpath -m -- "${CARGO_HOME:-$HOME/.cargo}/bin")"/*) echo 'Use checkout-built binaries, not the installation.' >&2; exit 2 ;;
  esac
done
module=${ATM_CHAT_TEST_PLAYWRIGHT:?Set ATM_CHAT_TEST_PLAYWRIGHT to the local Playwright index.mjs}
browsers=${PLAYWRIGHT_BROWSERS_PATH:?Set PLAYWRIGHT_BROWSERS_PATH to the local browser cache}
[[ $module == /*/playwright/index.mjs && -f $module && $browsers == /* && -d $browsers ]]
node=$(realpath -- "$(command -v node)")
[[ $("$node" -p 'process.versions.node.split(".")[0]') == 24 ]]
shots=${ATM_CHAT_TEST_SHOTS:+$(realpath -m -- "$ATM_CHAT_TEST_SHOTS")}
cd -- "$(dirname -- "$0")/../.."
dist=$(realpath -- "${ATM_CHAT_TEST_DIST:-dist}")
outer_net=$(readlink /proc/self/ns/net)
run=$(mktemp -d /var/tmp/atm-web-real.XXXXXX)
trap 'rm -rf -- "$run"' EXIT
mkdir -p "$run"/{s,home,tmp,os-tmp,tmux,state,xdg,config,tooling}
cp -a --reflink=auto -- "$(dirname -- "$(dirname -- "$module")")" "$run/tooling/node_modules"
cp -a --reflink=auto -- "$browsers" "$run/browsers"
env -i PATH="$(dirname -- "$node"):/usr/local/bin:/usr/bin:/bin" HOME="$run/home" SHELL=/bin/sh \
  TMPDIR="$run/tmp" TMUX_TMPDIR="$run/tmux" ATM_SOCKET="$run/s/atm.sock" ATM_TMUX_SOCKET=webchat \
  ATM_STATE_DIR="$run/state" XDG_STATE_HOME="$run/xdg" XDG_CONFIG_HOME="$run/config" \
  ATM_CHAT_TEST_RUN="$run" ATM_CHAT_TEST_DIST="$dist" ATM_CHAT_TEST_SHOTS="$shots" \
  ATM_CHAT_TEST_PLAYWRIGHT="$run/tooling/node_modules/playwright/index.mjs" PLAYWRIGHT_BROWSERS_PATH="$run/browsers" \
  LC_ALL=C.UTF-8 TZ=UTC timeout -k 8s 180s \
  unshare --map-root-user --net --pid --fork --mount-proc --kill-child bash -euc '
    test "$(readlink /proc/self/ns/net)" != "$1"; shift
    ip link set lo up
    test "$(ip -o link show | wc -l)" -eq 1
    # Chromium ignores lo for navigator.onLine. Give it an address on a
    # peerless dummy, without a prefix route or IPv6 link-local address.
    ip link add dev smoke-online type dummy
    ip link set dev smoke-online addrgenmode none
    ip addr add 192.0.2.1/32 dev smoke-online noprefixroute
    ip link set dev smoke-online up
    for family in -4 -6; do
      routes=$(ip "$family" route show table all type unicast)
      if [[ -n $routes ]]; then
        printf "Unexpected routes in smoke seal:\n%s\n" "$routes" >&2
        exit 1
      fi
    done
    mount --bind "$ATM_CHAT_TEST_RUN/os-tmp" /tmp
    test ! -e /tmp/atm.sock
    if command -v atm || command -v atmd; then echo "Installed ATM on sealed PATH" >&2; exit 1; fi
    exec node test/local/smoke.mjs --real "$@"
  ' seal "$outer_net" "$@"
