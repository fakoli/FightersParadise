#!/usr/bin/env bash
#
# fp.sh — process-control wrapper for the Fighters Paradise windowed game.
#
# A Makefile target runs a process in the foreground; it cannot cleanly
# launch, supervise, stop, or restart a long-running GUI. This script fills
# that gap for the SDL2/wgpu window: it can start fp-app detached, record its
# PID, and later stop/restart/status it. For one-shot builds and tests prefer
# the Makefile (`make build`, `make test`, ...); this script wraps those too so
# you have a single entry point during a play/iterate loop.
#
# Usage:
#   scripts/fp.sh <command> [args...]
#
# Commands:
#   build           Build the whole workspace (debug)
#   run [args...]   Run fp-app in the FOREGROUND (Ctrl-C to quit). With no args
#                   it loads the default two-KFM match (test pattern if absent).
#   start [args...] Build, then launch fp-app DETACHED; record PID + log
#   stop            Stop the detached fp-app (via recorded PID)
#   restart [args]  stop, then start (passes args through to start)
#   status          Report whether the detached fp-app is running
#   clean           cargo clean
#   test            Run the workspace test suite
#   lint            clippy with -D warnings (matches CI)
#
set -euo pipefail

# --- locate the repo root (this script lives in <root>/scripts) ---------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# --- runtime state (PID + log) ------------------------------------------------
# Prefer an in-repo, gitignored .run/ dir; fall back to /tmp if it can't be made.
RUN_DIR="${REPO_ROOT}/.run"
if ! mkdir -p "${RUN_DIR}" 2>/dev/null; then
    RUN_DIR="${TMPDIR:-/tmp}/fp-work-run"
    mkdir -p "${RUN_DIR}"
fi
PID_FILE="${RUN_DIR}/fp-app.pid"
LOG_FILE="${RUN_DIR}/fp-app.log"

# --- macOS SDL2 linker path (portable, no-op on Linux) ------------------------
# Mirror the Makefile: append Homebrew's libdir to RUSTFLAGS only when brew
# exists. Harmless duplicate of .cargo/config.toml's -L on Apple Silicon; also
# covers Intel macOS (/usr/local).
if BREW_PREFIX="$(brew --prefix 2>/dev/null)"; then
    export RUSTFLAGS="${RUSTFLAGS:-} -L ${BREW_PREFIX}/lib"
fi

DEFAULT_DEF="test-assets/kfm/kfm.def"

log()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# Read the recorded PID, but only echo it if that PID is actually alive.
# Cleans up a stale PID file. Echoes nothing (and returns 1) when not running.
running_pid() {
    [ -f "${PID_FILE}" ] || return 1
    local pid
    pid="$(cat "${PID_FILE}" 2>/dev/null || true)"
    if [ -z "${pid}" ]; then
        rm -f "${PID_FILE}"
        return 1
    fi
    if kill -0 "${pid}" 2>/dev/null; then
        printf '%s' "${pid}"
        return 0
    fi
    # stale: process gone but file lingered
    rm -f "${PID_FILE}"
    return 1
}

cmd_build() {
    log "building workspace (debug)"
    cargo build --workspace
}

cmd_run() {
    if [ "$#" -eq 0 ] && [ ! -e "${DEFAULT_DEF}" ]; then
        log "${DEFAULT_DEF} not found; launching no-arg test pattern"
    fi
    log "running fp-app in the foreground (Ctrl-C to quit)"
    exec cargo run -p fp-app -- "$@"
}

cmd_start() {
    local pid
    if pid="$(running_pid)"; then
        die "fp-app already running (pid ${pid}); use 'restart' or 'stop' first"
    fi

    # Build first so the window appears promptly and build errors surface here,
    # not silently in the detached log.
    cmd_build

    log "launching fp-app detached (log: ${LOG_FILE})"
    # Detach: redirect IO to the log, disown via nohup + background.
    nohup cargo run -p fp-app -- "$@" >"${LOG_FILE}" 2>&1 &
    local new_pid=$!
    echo "${new_pid}" >"${PID_FILE}"

    # Give it a beat; if it died instantly (bad args, no display), report it.
    sleep 1
    if kill -0 "${new_pid}" 2>/dev/null; then
        log "started (pid ${new_pid})"
    else
        rm -f "${PID_FILE}"
        warn "fp-app exited immediately — tail of ${LOG_FILE}:"
        tail -n 20 "${LOG_FILE}" >&2 || true
        die "failed to start fp-app"
    fi
}

cmd_stop() {
    local pid
    if ! pid="$(running_pid)"; then
        log "fp-app is not running (nothing to stop)"
        return 0
    fi
    log "stopping fp-app (pid ${pid})"
    kill "${pid}" 2>/dev/null || true

    # Wait up to ~5s for graceful exit, then escalate to SIGKILL.
    local waited=0
    while [ "${waited}" -lt 10 ]; do
        kill -0 "${pid}" 2>/dev/null || break
        sleep 0.5
        waited=$((waited + 1))
    done
    if kill -0 "${pid}" 2>/dev/null; then
        warn "did not exit gracefully; sending SIGKILL"
        kill -9 "${pid}" 2>/dev/null || true
    fi
    rm -f "${PID_FILE}"
    log "stopped"
}

cmd_restart() {
    cmd_stop
    cmd_start "$@"
}

cmd_status() {
    local pid
    if pid="$(running_pid)"; then
        log "fp-app is RUNNING (pid ${pid})"
        log "log: ${LOG_FILE}"
        return 0
    fi
    log "fp-app is NOT running"
    return 1
}

cmd_clean() {
    log "cargo clean"
    cargo clean
}

cmd_test() {
    log "running workspace tests"
    cargo test --workspace
}

cmd_lint() {
    log "clippy (deny warnings)"
    cargo clippy --workspace --all-targets -- -D warnings
}

usage() {
    sed -n '3,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

main() {
    local sub="${1:-}"
    [ "$#" -gt 0 ] && shift || true
    case "${sub}" in
        build)   cmd_build "$@" ;;
        run)     cmd_run "$@" ;;
        start)   cmd_start "$@" ;;
        stop)    cmd_stop "$@" ;;
        restart) cmd_restart "$@" ;;
        status)  cmd_status "$@" ;;
        clean)   cmd_clean "$@" ;;
        test)    cmd_test "$@" ;;
        lint)    cmd_lint "$@" ;;
        -h|--help|help|"") usage ;;
        *) warn "unknown command: ${sub}"; usage; exit 2 ;;
    esac
}

main "$@"
