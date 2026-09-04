#!/usr/bin/env bash
set -u

# test-init.sh — the template's anti-rot mechanism. Stamps a throwaway copy
# as "Ghost Note" and asserts the stamp is structurally sound. Runs inside
# the Scripts suite on every verify (and therefore on every push).
#
# Default mode is FAST: structural assertions + `cargo check` with a shared
# target dir (~seconds). `--full` additionally runs the stamped project's
# entire verify.sh and make build (minutes) — used by the release dress
# rehearsal and whenever init.sh itself changes.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
FULL=0
[ "${1:-}" = "--full" ] && FULL=1

# In a STAMPED project this test is not applicable: stamped repos refuse
# re-stamping by design, and their sentinel names are the project's real
# names. The test only guards the template itself.
if grep -q '^stamped = true' "$ROOT_DIR/.template-stamp.toml" 2>/dev/null; then
  echo "test-init: skipped (this is a stamped project, not the template)"
  exit 0
fi

PASS=0
FAIL=0

t() {
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "  ✗ $desc" >&2
  fi
}

WORK="$(mktemp -d /tmp/phantom-init-test.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
COPY="$WORK/copy"

# ---------------------------------------------------------------------------
# Make a clean copy of the template at HEAD (plus a .git so init.sh's
# dirty-tree preflight passes).
# ---------------------------------------------------------------------------
mkdir -p "$COPY"
if ! git -C "$ROOT_DIR" archive HEAD 2>/dev/null | tar -x -C "$COPY"; then
  echo "test-init: cannot archive HEAD; is this a git repo?" >&2
  exit 1
fi
git -C "$COPY" init -q -b main
git -C "$COPY" add -A
git -C "$COPY" -c user.email=test@test -c user.name=test commit -q -m "test copy"
COPY_HEAD="$(git -C "$COPY" rev-parse HEAD)"

# ---------------------------------------------------------------------------
# Stamp it (fast mode skips the stamped repo's own full verify)
# ---------------------------------------------------------------------------
STAMP_FLAGS=(--skip-verify)
[ "$FULL" = 1 ] && STAMP_FLAGS=()
if ! (cd "$COPY" && ./scripts/init.sh "Ghost Note" ${STAMP_FLAGS[@]+"${STAMP_FLAGS[@]}"} >"$WORK/init.log" 2>&1); then
  echo "test-init: init.sh FAILED; log follows:" >&2
  tail -30 "$WORK/init.log" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Structural assertions
# ---------------------------------------------------------------------------
t "no sentinel forms survive anywhere" \
  bash -c "! grep -rqE 'phantom|phantom|Phantom|phantom|PHANTOM|Phantom|Phantom' '$COPY' \
    --exclude-dir=.git --exclude-dir=target --exclude-dir=.build --exclude-dir=.beads --exclude-dir=.runtime"
t "build-system Config package renamed (default = Pascal)" \
  grep -q 'package.GhostNote =' "$COPY/Config"
t "crate dir renamed" test -d "$COPY/rust/ghost-note-core"
t "swift source renamed" test -d "$COPY/swift/Sources/GhostNote"
t "workspace references new crates" grep -q 'ghost-note-core' "$COPY/rust/Cargo.toml"
t "stamp identity recorded" grep -q '^stamped = true' "$COPY/.template-stamp.toml"
t "stamp records template commit" \
  grep -q "template_commit = \"$COPY_HEAD\"" "$COPY/.template-stamp.toml"
t "git history re-initialized (single-digit commits)" \
  bash -c "[ \$(git -C '$COPY' rev-list --count HEAD) -le 3 ]"
t "pre-push gate installed" grep -q 'GHOST_NOTE VERIFY GATE' "$COPY/.git/hooks/pre-push"
t "check-hooks passes in the stamped repo" env -C "$COPY" ./scripts/check-hooks.sh
t "env prefix stamped in config" grep -q 'GHOST_NOTE_PROFILE' "$COPY/rust/ghost-note-api/src/config.rs"
t "derived port stamped (no 8768 left)" \
  bash -c "! grep -rq 8768 '$COPY/rust' --include='*.rs'"

# The stamped Rust workspace must still typecheck. Shared target dir reuses
# the template's dependency builds, so this is seconds, not minutes.
# --all-targets so tests/benches compile too — fast mode otherwise misses
# breakage that only surfaces when test code is built (adversarial finding,
# 2026-08-20). Shared target dir keeps this in seconds.
t "stamped rust workspace passes cargo check" \
  env CARGO_TARGET_DIR="$ROOT_DIR/rust/target" bash -c "cd '$COPY/rust' && cargo check --workspace --all-targets --quiet"

# Prune combos: website- and swift-less stamps must also hold together.
COPY2="$WORK/copy2"
mkdir -p "$COPY2"
git -C "$ROOT_DIR" archive HEAD | tar -x -C "$COPY2"
git -C "$COPY2" init -q -b main && git -C "$COPY2" add -A
git -C "$COPY2" -c user.email=t@t -c user.name=t commit -q -m "test copy"
# Also exercises --package with an explicit build-system-style name.
if (cd "$COPY2" && ./scripts/init.sh "Bare Bones" --package AcmeBareBones --no-website --no-ope --no-swift --skip-verify >"$WORK/init2.log" 2>&1); then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  echo "  ✗ pruned stamp failed; log tail:" >&2
  tail -15 "$WORK/init2.log" >&2
fi
t "pruned layers are gone" \
  bash -c "[ ! -d '$COPY2/website' ] && [ ! -d '$COPY2/swift' ] && [ ! -d '$COPY2/open-prompt-edition' ]"
t "pruned stamp left no sentinels" \
  bash -c "! grep -rqE 'phantom|Phantom|PHANTOM|Phantom' '$COPY2' --exclude-dir=.git --exclude-dir=.beads"
t "--package override applied to build-system Config" \
  grep -q 'package.AcmeBareBones =' "$COPY2/Config"
t "verify.sh in pruned copy skips missing layers and passes scripts suite" \
  bash -c "cd '$COPY2' && ./scripts/verify.sh --scripts-only"

# Re-stamping a stamped copy must refuse.
t "double-stamp is refused" \
  bash -c "! (cd '$COPY' && ./scripts/init.sh 'Second Try' --skip-verify)"

# ---------------------------------------------------------------------------
# Full mode already ran the stamped verify inside init.sh; say so.
# ---------------------------------------------------------------------------
if [ "$FULL" = 1 ]; then
  echo "test-init: full mode — stamped project passed its own verify.sh and make build"
fi

echo "test-init: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
