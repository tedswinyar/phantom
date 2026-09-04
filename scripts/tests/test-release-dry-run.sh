#!/usr/bin/env bash
set -u

# DRY_RUN=1 release.sh must be provably incapable of mutating git state: no
# commit, no tag, no push. Grep is too weak a proof, so the seam is a git
# WRAPPER on PATH that records every mutating subcommand (add/commit/tag/
# push) to a file and delegates everything else to the real git — the REAL
# release.sh then runs end-to-end in a sandbox repo (heavy externals
# stubbed: verify, DMG, notes, cliff/about/cyclonedx). The dry run must
# leave the record EMPTY; a control run WITHOUT DRY_RUN must record the tag,
# proving the wrapper actually intercepts (a test whose seam is inert would
# green-light anything).

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

WORK="$(mktemp -d /tmp/phantom-dry-run.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
R="$WORK/repo"
RECORD="$WORK/git-mutations"
REAL_GIT="$(command -v git)"

# --- Sandbox repo with just enough shape for release.sh's gates -------------
mkdir -p "$R/scripts" "$R/swift/Sources/Phantom" "$R/rust" "$R/website/content"
cp "$SCRIPT_DIR/release.sh" "$R/scripts/"
chmod +x "$R/scripts/release.sh"
# The marketing line ends at the closing quote, matching the real file's
# shape (release.sh's sed leaves anything after the quote in the capture).
printf 'enum V {\n    static let marketing = "9.9.9"\n}\n' > "$R/swift/Sources/Phantom/Version.swift"
mkdir -p "$R/open-prompt-edition"
printf '9.9.9\n' > "$R/open-prompt-edition/VERSION"

# Heavy gates stubbed: this test proves the DRY_RUN guard, not the gates
# (they have their own suites).
for stub in verify.sh build-dmg.sh; do
  printf '#!/bin/sh\nexit 0\n' > "$R/scripts/$stub"
  chmod +x "$R/scripts/$stub"
done
cat > "$R/scripts/generate-release-notes.sh" <<'EOF'
#!/bin/sh
echo notes > CHANGELOG.md
mkdir -p website/content
echo notes > website/content/changelog.md
exit 0
EOF
chmod +x "$R/scripts/generate-release-notes.sh"

# --- PATH shims --------------------------------------------------------------
mkdir -p "$WORK/shims"
# Release tooling presence checks + invocations.
for tool in git-cliff cargo-about cargo-cyclonedx; do
  printf '#!/bin/sh\nexit 0\n' > "$WORK/shims/$tool"
  chmod +x "$WORK/shims/$tool"
done
# cargo shim: `cargo about generate … -o ../THIRD-PARTY-NOTICES.html` and
# `cargo cyclonedx --format json` produce the files release.sh checks for.
cat > "$WORK/shims/cargo" <<'EOF'
#!/bin/sh
case "$1" in
  about)
    touch ../THIRD-PARTY-NOTICES.html
    ;;
  cyclonedx)
    mkdir -p phantom-core
    echo '{}' > phantom-core/phantom-core.cdx.json
    ;;
esac
exit 0
EOF
chmod +x "$WORK/shims/cargo"

# THE SEAM: a git wrapper that records mutating subcommands and passes
# everything else (status, rev-parse, diff, …) to the real git.
cat > "$WORK/shims/git" <<EOF
#!/bin/sh
case "\$1" in
  add|commit|tag|push)
    echo "\$@" >> "$RECORD"
    exit 0
    ;;
esac
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$WORK/shims/git"

SHIM_PATH="$WORK/shims:/usr/bin:/bin"

git -C "$R" init -q -b main
git -C "$R" -c user.email=t@t -c user.name=t add -A
git -C "$R" -c user.email=t@t -c user.name=t commit -qm base

# --- DRY_RUN: the guard must skip every mutating git operation --------------
# env -u on both legs: when the OUTER invocation is itself a DRY_RUN
# release rehearsal (DRY_RUN=1 release.sh -> verify -> this test), the
# inherited variable would turn the control run into a second dry run and
# fail its own assertions (hermeticity bug, found 2026-09-03).
: > "$RECORD"
OUT="$(cd "$R" && env -u SKIP_NOTARIZE PATH="$SHIM_PATH" DRY_RUN=1 ./scripts/release.sh 9.9.9 2>&1)"
STATUS=$?
t "dry run exits 0" [ "$STATUS" -eq 0 ]
t "dry run announces the skip" \
  bash -c "echo \"\$1\" | grep -q 'DRY_RUN: skipping commit and tag'" _ "$OUT"
t "dry run invoked NO mutating git command" [ ! -s "$RECORD" ]
t "no tag exists after the dry run" \
  bash -c "! '$REAL_GIT' -C '$R' rev-parse v9.9.9 >/dev/null 2>&1"

# --- Control: WITHOUT DRY_RUN the same run must hit the tag path, proving
# the wrapper seam intercepts for real. (Commit the dry run's artifact
# litter first — a real release refuses a dirty tree.)
git -C "$R" -c user.email=t@t -c user.name=t add -A
git -C "$R" -c user.email=t@t -c user.name=t commit -qm artifacts
: > "$RECORD"
OUT="$(cd "$R" && env -u DRY_RUN -u SKIP_NOTARIZE PATH="$SHIM_PATH" ./scripts/release.sh 9.9.9 2>&1)"
STATUS=$?
t "control run (no DRY_RUN) completes" [ "$STATUS" -eq 0 ]
t "control run reached git tag (the seam intercepts)" \
  grep -q '^tag ' "$RECORD"
t "control run reached git add" grep -q '^add ' "$RECORD"

echo "test-release-dry-run: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
