#!/usr/bin/env bash
# verify.sh — run the repo health checks from one entry point.
#
# Usage:
#   ./scripts/verify.sh [--rust-only|--swift-only|--scripts-only]
#
# Suites are auto-detected by directory presence, so pruned layers (init.sh
# --no-swift etc.) need no script surgery:
#   rust      rust/                 cargo test --workspace
#   swift     swift/                swift test
#   e2e       tests/e2e/            run-e2e.sh parity harness
#   scripts   scripts/tests/        the tests of the gates themselves
#   ope       open-prompt-edition/  version guard + conformance
#   website   website/              hugo build (config sanity)
#
# Concurrency: only one verify.sh may run at a time. Two concurrent swift
# builds against the same checkout contend on .build/ and fail confusingly.
# A second invocation exits immediately with code 75 and a clear message.
#
# Logs: each suite tees to .runtime/verify/<suite>.log; the console shows a
# one-line PASS/FAIL per suite and a final summary.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="$REPO_ROOT/.runtime/verify"
LOCK_DIR="$REPO_ROOT/.runtime/verify.lock"
LOCK_EXIT_CODE=75

# Deliberately INLINE rather than sourced from scripts/lib/completion-guard.sh: a
# gate must not depend on another file being present in order to report failure
# correctly, and the sandbox tests copy this script out on its own. The library
# carries the same logic for every other script in the repo, and
# scripts/tests/test-completion-guard.sh pins its behaviour.
#
# On macOS bash 3.2 — the only bash on this machine, and what `#!/usr/bin/env bash`
# resolves to — a set -u abort in a script that has an EXIT trap delivers status 0
# to the trap. Without this guard verify.sh would crash and exit 0, and the
# pre-push hook would wave the push through while printing a bash error nobody
# reads. Armed below before argument parsing, which is otherwise unprotected.
COMPLETED=0
PREMATURE_EXIT_CODE=70

# Every DELIBERATE exit goes through finish(). A bare `exit` is reported as
# premature, which is the point: crashing into an exit and choosing one stay
# distinguishable. 70 stays distinct from 1 (real suite failure) and 75 (lock).
finish() { COMPLETED=1; exit "$1"; }

# Only release a lock this process owns. The guard is armed before acquire_lock, so
# an early exit (--help, unknown option) must not delete a concurrent run's lock.
release_lock() {
  if [ -f "$LOCK_DIR/pid" ] && [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" = "$$" ]; then
    rm -rf "$LOCK_DIR"
  fi
  return 0
}

on_exit() {
  local s=$?
  release_lock
  if [ "$COMPLETED" != "1" ]; then
    echo "verify.sh: exited before finishing (reported status $s) — treating as FAILURE." >&2
    echo "verify.sh: see the completion-guard comment in this script." >&2
    [ "$s" != "0" ] && exit "$s"
    exit "$PREMATURE_EXIT_CODE"
  fi
  exit "$s"
}
trap on_exit EXIT


ONLY=""
case "${1:-}" in
  --rust-only)    ONLY="rust" ;;
  --swift-only)   ONLY="swift" ;;
  --scripts-only) ONLY="scripts" ;;
  -h|--help)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    finish 0
    ;;
  "") ;;
  *) echo "verify.sh: unknown option: $1" >&2; finish 2 ;;
esac

mkdir -p "$LOG_DIR"

# ---------------------------------------------------------------------------
# Lock (exit 75 if another verify.sh is running; reclaim if the holder died)
# ---------------------------------------------------------------------------
acquire_lock() {
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    echo $$ > "$LOCK_DIR/pid"
    return 0
  fi
  local holder
  holder="$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")"
  if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
    echo "verify.sh: another verify run (pid $holder) holds the lock; exiting 75." >&2
    echo "verify.sh: do not run cargo/swift test by hand while it runs." >&2
    echo "verify.sh: if no verify is actually running (check: pgrep -f verify.sh)," >&2
    echo "verify.sh: the holder crashed and pid $holder was recycled — remove" >&2
    echo "verify.sh: $LOCK_DIR and retry." >&2
    finish "$LOCK_EXIT_CODE"
  fi
  echo "verify.sh: reclaiming stale lock (holder ${holder:-unknown} is gone)." >&2
  rm -rf "$LOCK_DIR"
  mkdir "$LOCK_DIR" 2>/dev/null || {
    echo "verify.sh: lost the lock race; exiting 75." >&2
    finish "$LOCK_EXIT_CODE"
  }
  echo $$ > "$LOCK_DIR/pid"
}

acquire_lock

# ---------------------------------------------------------------------------
# Suite runner
# ---------------------------------------------------------------------------
PASSED=()
FAILED=()
SKIPPED=()

run_suite() {
  # run_suite <name> <dir-that-must-exist> <command...>
  local name="$1" gate_dir="$2"
  shift 2
  if [ -n "$ONLY" ] && [ "$ONLY" != "$name" ]; then
    return 0
  fi
  if [ ! -d "$REPO_ROOT/$gate_dir" ]; then
    SKIPPED+=("$name")
    return 0
  fi
  local log="$LOG_DIR/$name.log"
  printf '── %-8s ' "$name"
  local start end
  start=$(date +%s)
  if ( cd "$REPO_ROOT" && "$@" ) >"$log" 2>&1; then
    end=$(date +%s)
    echo "PASS  ($((end - start))s, log: ${log#"$REPO_ROOT"/})"
    PASSED+=("$name")
  else
    end=$(date +%s)
    echo "FAIL  ($((end - start))s)"
    echo "   ↳ tail of ${log#"$REPO_ROOT"/}:"
    tail -n 15 "$log" | sed 's/^/   │ /'
    FAILED+=("$name")
  fi
}

run_rust() {
  (cd rust && cargo test --workspace) || return 1
  # Advisory + license gate. Locally a missing tool is a loud warning, not
  # a brick (doctor.sh nags); in CI (CI env var set — GitHub sets CI=true)
  # a missing tool FAILS the run, because CI installs its tools explicitly
  # and a silent skip there would make the gate decorative.
  if command -v cargo-deny >/dev/null; then
    (cd rust && cargo deny check advisories licenses sources) || return 1
  elif [ -n "${CI:-}" ]; then
    echo "ERROR: CI is set but cargo-deny is not installed — the advisory/license gate must run in CI." >&2
    return 1
  else
    echo "WARNING: cargo-deny not installed — advisory/license gate SKIPPED (brew install cargo-deny)" >&2
  fi
}
run_swift()   { (cd swift && swift test); }
run_e2e()     { tests/e2e/run-e2e.sh; }
run_scripts() {
  local rc=0 t
  for t in scripts/tests/test-*.sh; do
    [ -e "$t" ] || continue
    echo "── scripts suite: $t"
    "$t" || rc=1
  done
  # Secret scan over the working tree. Same posture as cargo-deny: locally
  # present-or-warn so a fresh clone still builds; in CI a missing tool
  # FAILS (the workflow installs it — absence there means the install step
  # regressed, and .gitleaks.toml must never be dead config in the gate).
  if [ -f .gitleaks.toml ]; then
    if command -v gitleaks >/dev/null; then
      echo "── scripts suite: gitleaks secret scan"
      gitleaks detect --no-banner --config .gitleaks.toml || rc=1
    elif [ -n "${CI:-}" ]; then
      echo "ERROR: CI is set but gitleaks is not installed — the secret scan must run in CI." >&2
      rc=1
    else
      echo "WARNING: gitleaks not installed — secret scan SKIPPED (brew install gitleaks)" >&2
    fi
  fi
  return "$rc"
}
# The OPE version-bump guard is a PUSH-time check (it diffs base..head and
# runs inside the pre-push hook with real SHAs); here we run only the
# conformance harness.
run_ope() {
  if [ -x open-prompt-edition/kit/09-conformance/run.sh ]; then
    open-prompt-edition/kit/09-conformance/run.sh
  fi
}
run_website() { (cd website && hugo build --quiet --destination "$REPO_ROOT/.runtime/verify/hugo-out"); }

echo "verify.sh — $(date '+%Y-%m-%d %H:%M:%S')  (logs: .runtime/verify/)"
run_suite rust    rust                 run_rust
run_suite swift   swift                run_swift
run_suite e2e     tests/e2e            run_e2e
run_suite scripts scripts/tests        run_scripts
run_suite ope     open-prompt-edition  run_ope
run_suite website website              run_website

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  echo "skipped (layer not present): ${SKIPPED[*]}"
fi
if [ "${#FAILED[@]}" -gt 0 ]; then
  echo "FAILED: ${FAILED[*]}"
  finish 1
fi
echo "all suites passed: ${PASSED[*]:-none}"
finish 0
