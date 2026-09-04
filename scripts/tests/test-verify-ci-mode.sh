#!/usr/bin/env bash
set -u

# verify.sh's CI mode: with CI set (GitHub sets CI=true), a missing
# cargo-deny or gitleaks FAILS the run instead of warn-and-skip — CI
# installs its tools explicitly, so absence there means the install step
# regressed and the gate must not go quietly decorative. Locally (CI unset)
# the warn-and-skip posture is unchanged. Proven by EXECUTING the real
# verify.sh in a sandbox whose PATH controls which tools exist.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
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

WORK="$(mktemp -d /tmp/phantom-ci-mode.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/scripts/tests" "$WORK/rust" "$WORK/shims"
cp "$SCRIPT_DIR/verify.sh" "$WORK/scripts/"
chmod +x "$WORK/scripts/verify.sh"

# The layers under test: rust (cargo-deny arm) and scripts (gitleaks arm).
cat > "$WORK/scripts/tests/test-noop.sh" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$WORK/scripts/tests/test-noop.sh"
touch "$WORK/.gitleaks.toml"

# cargo shim: `cargo test --workspace` passes, so the run reaches the
# cargo-deny arm. (cargo-deny is a SEPARATE binary; its absence from PATH
# is the condition under test.)
cat > "$WORK/shims/cargo" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$WORK/shims/cargo"

# A PATH with core tools but WITHOUT cargo-deny/gitleaks (they live in the
# homebrew prefix, which is deliberately excluded).
BARE_PATH="$WORK/shims:/usr/bin:/bin"

# --- CI set + tools missing: both arms must FAIL loudly --------------------
OUT="$(cd "$WORK" && env PATH="$BARE_PATH" CI=1 ./scripts/verify.sh 2>&1)"
STATUS=$?
t "CI + missing tools exits nonzero" [ "$STATUS" -ne 0 ]
t "rust suite fails on missing cargo-deny" \
  bash -c "echo \"\$1\" | grep -q 'FAILED:.*rust'" _ "$OUT"
t "scripts suite fails on missing gitleaks" \
  bash -c "echo \"\$1\" | grep -q 'FAILED:.*scripts'" _ "$OUT"
t "the cargo-deny error names the tool and CI" \
  bash -c "echo \"\$1\" | grep -q 'CI is set but cargo-deny'" _ "$OUT"
t "the gitleaks error names the tool and CI" \
  bash -c "echo \"\$1\" | grep -q 'CI is set but gitleaks'" _ "$OUT"

# --- CI unset + tools missing: local warn-and-skip posture unchanged -------
OUT="$(cd "$WORK" && env -u CI PATH="$BARE_PATH" ./scripts/verify.sh 2>&1)"
STATUS=$?
t "locally, missing tools still pass (warn-and-skip)" [ "$STATUS" -eq 0 ]
# Warnings land in the per-suite logs (a passing suite prints no tail).
t "the local run warns about cargo-deny" \
  grep -q 'WARNING: cargo-deny not installed' "$WORK/.runtime/verify/rust.log"
t "the local run warns about gitleaks" \
  grep -q 'WARNING: gitleaks not installed' "$WORK/.runtime/verify/scripts.log"

# --- CI set + tools present: CI mode is fail-closed, not fail-always -------
cat > "$WORK/shims/cargo-deny" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$WORK/shims/gitleaks" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$WORK/shims/cargo-deny" "$WORK/shims/gitleaks"
t "CI with tools present passes" \
  bash -c "cd '$WORK' && env PATH='$BARE_PATH' CI=1 ./scripts/verify.sh"

# --- and a FAILING tool still fails the suite in both modes ----------------
cat > "$WORK/shims/gitleaks" <<'EOF'
#!/bin/sh
echo "leak found (simulated)"
exit 1
EOF
chmod +x "$WORK/shims/gitleaks"
OUT="$(cd "$WORK" && env -u CI PATH="$BARE_PATH" ./scripts/verify.sh 2>&1)"
t "a failing gitleaks fails the local run too" [ "$?" -ne 0 ]
t "the failure surfaces the gitleaks output" \
  bash -c "echo \"\$1\" | grep -q 'leak found (simulated)'" _ "$OUT"

echo "test-verify-ci-mode: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
