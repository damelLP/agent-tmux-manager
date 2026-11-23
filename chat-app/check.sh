#!/bin/sh
# Use the real toolchain directory; rustup's cargo shim can put ~/.cargo/bin on PATH.
set -eu
app=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(dirname -- "$app")
cd "$app/bridge"
cargo=$(rustup which cargo)
run=$(mktemp -d /var/tmp/atm-chat.XXXXXX)
trap 'rm -rf -- "$run"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$run/s" "$run/home" "$run/tmp" "$run/tmux" "$run/state" "$run/xdg" "$run/config"

unset TMUX TMUX_PANE ATM_TMUX_SOCKET CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR RUSTC RUSTC_WRAPPER
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export PATH="$(dirname -- "$cargo"):/usr/local/bin:/usr/bin:/bin"
export HOME="$run/home" SHELL=/bin/sh TMPDIR="$run/tmp" TMUX_TMPDIR="$run/tmux"
export ATM_SOCKET="$run/s/atm.sock" ATM_STATE_DIR="$run/state"
export XDG_STATE_HOME="$run/xdg" XDG_CONFIG_HOME="$run/config"
case $(command -v atmd || true) in
    "$CARGO_HOME"/bin/*) echo 'Refusing an installed atmd on tool PATH' >&2; exit 1 ;;
esac

if [ -z "${ATM_CHAT_TEST_ATMD:-}" ]; then
    if [ ! -f "$repo/src/bin/atmd.rs" ]; then
        echo 'Set ATM_CHAT_TEST_ATMD to the absolute path of a compatible atmd executable' >&2
        exit 1
    fi
    nice -n 10 "$cargo" build --locked --manifest-path "$repo/Cargo.toml" --target-dir "$repo/target" -p atm --bin atmd
    ATM_CHAT_TEST_ATMD="$repo/target/debug/atmd"
fi
case "$ATM_CHAT_TEST_ATMD" in
    /*) test -f "$ATM_CHAT_TEST_ATMD" && test -x "$ATM_CHAT_TEST_ATMD" || exit 1 ;;
    *) echo 'ATM_CHAT_TEST_ATMD must be an absolute executable path' >&2; exit 1 ;;
esac
export ATM_CHAT_TEST_ATMD

nice -n 10 "$cargo" fmt --all --check
nice -n 10 "$cargo" clippy --locked --workspace --all-targets -- -D warnings
nice -n 10 "$cargo" test --locked --workspace

# The page build uses only locked dev tooling, with a private npm environment.
(
    cd "$app/web"
    case ${ATM_CHAT_NODE_BIN:-} in
        '') ;;
        /*) test -x "$ATM_CHAT_NODE_BIN/node" && test -x "$ATM_CHAT_NODE_BIN/npm" || exit 1
            PATH="$ATM_CHAT_NODE_BIN:$PATH" ;;
        *) echo 'Set ATM_CHAT_NODE_BIN to an absolute Node 24 bin directory' >&2; exit 1 ;;
    esac
    env -i PATH="$PATH" HOME="$HOME" SHELL=/bin/sh TMPDIR="$TMPDIR" \
        ATM_SOCKET="$ATM_SOCKET" ATM_STATE_DIR="$ATM_STATE_DIR" TMUX_TMPDIR="$TMUX_TMPDIR" \
        XDG_STATE_HOME="$XDG_STATE_HOME" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" \
        npm_config_cache="$run/npm" npm_config_userconfig="$run/config/npm-user" \
        npm_config_globalconfig="$run/config/npm-global" /bin/sh -ec '
            if ! node -e "process.exit(process.versions.node.split(\".\")[0] === \"24\" ? 0 : 1)"; then
                echo "ATM Chat web checks require Node 24 (set ATM_CHAT_NODE_BIN)" >&2; exit 1
            fi
            npm ci --include=dev --ignore-scripts --no-audit --no-fund
            npm run build
            npm test
            npm run size
        '
)
