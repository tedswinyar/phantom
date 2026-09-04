#!/usr/bin/env bash
set -euo pipefail

# init.sh — stamp a fresh copy of spooky-shell into a new project.
#
# Usage:
#   ./scripts/init.sh "Ghost Note" [options]
#
# Options:
#   --domain <host>   Website domain (sets Hugo baseURL); else a TODO marker
#   --port <n>        API port (default: derived from the name, 18000-18999)
#   --no-website      Prune the website layer
#   --no-ope          Prune the open-prompt-edition layer
#   --no-swift        Prune the Swift app layer
#   --skip-verify     Skip the final verify.sh + make build proof
#                     (used by test-init.sh; real stamps should not pass this)
#
# The template ships as a WORKING app named "Phantom"; stamping is a
# blind rename of its five name forms (see lib/names.sh). A stamp that does
# not build is a failed stamp: init.sh exits nonzero if the stamped
# project's own verify.sh fails.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

info() { printf 'init: %s\n' "$*"; }
die() {
  printf 'init: ERROR: %s\n' "$*" >&2
  exit 1
}

# shellcheck source=lib/names.sh
. "$SCRIPT_DIR/lib/names.sh"

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
[ $# -ge 1 ] || die "usage: ./scripts/init.sh \"Project Name\" [--domain host] [--port n] [--no-website] [--no-ope] [--no-swift]"
PROJECT_NAME="$1"
shift

DOMAIN=""
PORT_OVERRIDE=""
PACKAGE_OVERRIDE=""
PRUNE_WEBSITE=0
PRUNE_OPE=0
PRUNE_SWIFT=0
SKIP_VERIFY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN="${2:?--domain needs a value}"; shift 2 ;;
    --port) PORT_OVERRIDE="${2:?--port needs a value}"; shift 2 ;;
    --package) PACKAGE_OVERRIDE="${2:?--package needs a value}"; shift 2 ;;
    --no-website) PRUNE_WEBSITE=1; shift ;;
    --no-ope) PRUNE_OPE=1; shift ;;
    --no-swift) PRUNE_SWIFT=1; shift ;;
    --skip-verify) SKIP_VERIFY=1; shift ;;
    *) die "unknown option: $1" ;;
  esac
done

phantom_derive_names "$PROJECT_NAME" || die "cannot derive names from '$PROJECT_NAME'"
PORT="${PORT_OVERRIDE:-$NAME_DEFAULT_PORT}"
case "$PORT" in
  *[!0-9]*|"") die "--port must be a number: '$PORT'" ;;
esac
DEV_PORT=$((PORT + 10))

# The build-system package name (root `Config`). This is a SEPARATE naming
# axis from the app sentinel — the template's package is `Phantom`
# (named after the repo, not the demo app). It cannot be derived, because the
# build-system package name is whatever package you created for this project
# (often a team-prefixed name). Default to the Pascal app name; pass
# --package to match your real package.
PACKAGE="${PACKAGE_OVERRIDE:-$NAME_PASCAL}"
case "$PACKAGE" in
  [A-Za-z]*) ;;
  *) die "--package must start with a letter: '$PACKAGE'" ;;
esac
if printf '%s' "$PACKAGE" | LC_ALL=C grep -q '[^A-Za-z0-9]'; then
  die "--package must be alphanumeric (a build-system package name): '$PACKAGE'"
fi

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
[ -f "$ROOT_DIR/.template-stamp.toml" ] || die "no .template-stamp.toml — is this a spooky-shell copy?"
if ! grep -q '^stamped = false' "$ROOT_DIR/.template-stamp.toml"; then
  die "this copy is already stamped (see .template-stamp.toml); stamp from a fresh copy of spooky-shell"
fi

for tool in cargo jq git; do
  command -v "$tool" >/dev/null || die "required tool not found: $tool"
done
[ "$PRUNE_SWIFT" = 1 ] || command -v swift >/dev/null || die "swift not found (or pass --no-swift)"
[ "$PRUNE_WEBSITE" = 1 ] || command -v hugo >/dev/null || die "hugo not found (or pass --no-website)"
command -v bd >/dev/null || info "WARNING: bd (beads) not found; skipping issue-tracker setup"

if [ -d "$ROOT_DIR/.git" ]; then
  if ! git -C "$ROOT_DIR" diff --quiet HEAD 2>/dev/null || \
     [ -n "$(git -C "$ROOT_DIR" status --porcelain 2>/dev/null)" ]; then
    die "working tree is dirty; stamp from a clean copy"
  fi
fi

TEMPLATE_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"
STAMP_DATE="$(date +%Y-%m-%d)"

info "stamping '$NAME_HUMAN' (slug: $NAME_SLUG, port: $PORT)"

# ---------------------------------------------------------------------------
# Prune optional layers (pure deletion; verify.sh auto-detects layers)
# ---------------------------------------------------------------------------
if [ "$PRUNE_WEBSITE" = 1 ]; then
  info "pruning website layer"
  rm -rf "$ROOT_DIR/website"
  rm -f "$ROOT_DIR/scripts/publish.sh" "$ROOT_DIR/scripts/capture-screenshots.sh"
fi
if [ "$PRUNE_OPE" = 1 ]; then
  info "pruning open-prompt-edition layer"
  rm -rf "$ROOT_DIR/open-prompt-edition"
  rm -f "$ROOT_DIR/scripts/check-ope-version-bump.sh" \
        "$ROOT_DIR/scripts/tests/test-ope-version-guard.sh"
fi
if [ "$PRUNE_SWIFT" = 1 ]; then
  info "pruning swift layer"
  rm -rf "$ROOT_DIR/swift"
  rm -f "$ROOT_DIR/scripts/build-app.sh" "$ROOT_DIR/scripts/build-dmg.sh" \
        "$ROOT_DIR/scripts/dev-build.sh" "$ROOT_DIR/skills/swift-conventions.md"
fi

# ---------------------------------------------------------------------------
# Rename paths carrying sentinel forms
# ---------------------------------------------------------------------------
# Deepest paths first so parent renames don't orphan children.
find "$ROOT_DIR" \
    -name .git -prune -o \
    -name target -prune -o \
    -name .build -prune -o \
    -name build -prune -o \
    -name dist -prune -o \
    -name .beads -prune -o \
    -name .runtime -prune -o \
    -depth \( -name '*phantom*' -o -name '*phantom*' -o -name '*Phantom*' -o -name '*phantom*' \) -print | \
while IFS= read -r path; do
  base="$(basename "$path")"
  new_base="$(printf '%s' "$base" | sed \
    -e "s/phantom/$NAME_SLUG/g" \
    -e "s/phantom/$NAME_SNAKE/g" \
    -e "s/Phantom/$NAME_PASCAL/g" \
    -e "s/phantom/$NAME_COMPACT/g")"
  if [ "$base" != "$new_base" ]; then
    mv "$path" "$(dirname "$path")/$new_base"
  fi
done

# ---------------------------------------------------------------------------
# Rewrite file contents (five forms + human name + ports)
# ---------------------------------------------------------------------------
# Blind global replacement is safe because the five forms are textually
# distinct; PHANTOM must run before phantom-insensitive matches is a
# non-issue since sed is case-sensitive. Ports are distinct literals.
find "$ROOT_DIR" \
    -name .git -prune -o \
    -name target -prune -o \
    -name .build -prune -o \
    -name build -prune -o \
    -name dist -prune -o \
    -name .beads -prune -o \
    -name .runtime -prune -o \
    -name '*.png' -prune -o \
    -type f -print | \
while IFS= read -r f; do
  # Skip binaries defensively (grep -Iq exits 0 only for text files).
  grep -Iq . "$f" 2>/dev/null || continue
  if LC_ALL=C grep -qE 'phantom|phantom|Phantom|phantom|PHANTOM|Phantom|Phantom|8768|8778' "$f"; then
    sed -i '' \
      -e "s/PHANTOM/$NAME_SCREAMING/g" \
      -e "s/phantom/$NAME_SLUG/g" \
      -e "s/phantom/$NAME_SNAKE/g" \
      -e "s/Phantom/$NAME_PASCAL/g" \
      -e "s/phantom/$NAME_COMPACT/g" \
      -e "s/Phantom/$NAME_HUMAN/g" \
      -e "s/Phantom/$PACKAGE/g" \
      -e "s/8768/$PORT/g" \
      -e "s/8778/$DEV_PORT/g" \
      "$f"
  fi
done

# ---------------------------------------------------------------------------
# Website identity
# ---------------------------------------------------------------------------
if [ "$PRUNE_WEBSITE" = 0 ] && [ -f "$ROOT_DIR/website/hugo.yaml" ]; then
  if [ -n "$DOMAIN" ]; then
    sed -i '' "s|^baseURL:.*|baseURL: https://$DOMAIN/|" "$ROOT_DIR/website/hugo.yaml"
  else
    sed -i '' "s|^baseURL:.*|baseURL: https://TODO-stamp-set-your-domain.example.com/|" "$ROOT_DIR/website/hugo.yaml"
    info "website baseURL left as a TODO marker (pass --domain to set it)"
  fi
fi

# ---------------------------------------------------------------------------
# Identity file
# ---------------------------------------------------------------------------
cat > "$ROOT_DIR/.template-stamp.toml" <<EOF
# Written by spooky-shell's init.sh. Records where this project came from so
# improvements can flow both ways (see TEMPLATE.md's backport protocol).
stamped = true
name = "$NAME_HUMAN"
slug = "$NAME_SLUG"
template = "spooky-shell"
template_commit = "$TEMPLATE_COMMIT"
stamp_date = "$STAMP_DATE"
api_port = $PORT
package = "$PACKAGE"
layers_pruned = [$(
  first=1
  for l in $( [ "$PRUNE_WEBSITE" = 1 ] && echo website; [ "$PRUNE_OPE" = 1 ] && echo ope; [ "$PRUNE_SWIFT" = 1 ] && echo swift ); do
    [ "$first" = 1 ] || printf ', '
    printf '"%s"' "$l"
    first=0
  done
)]
EOF

# ---------------------------------------------------------------------------
# Leftover-sentinel sweep: a stamped tree must contain NO sentinel forms.
# ---------------------------------------------------------------------------
LEFTOVERS="$(grep -rlE 'phantom|phantom|Phantom|phantom|PHANTOM|Phantom|Phantom' "$ROOT_DIR" \
  --exclude-dir=.git --exclude-dir=target --exclude-dir=.build --exclude-dir=build --exclude-dir=dist \
  --exclude-dir=.beads --exclude-dir=.runtime 2>/dev/null || true)"
if [ -n "$LEFTOVERS" ]; then
  printf '%s\n' "$LEFTOVERS" >&2
  die "sentinel forms survived the stamp in the files above"
fi

# ---------------------------------------------------------------------------
# Re-init git, beads, hooks
# ---------------------------------------------------------------------------
info "re-initializing git history"
rm -rf "$ROOT_DIR/.git"
git -C "$ROOT_DIR" init -q -b main
git -C "$ROOT_DIR" add -A
git -C "$ROOT_DIR" commit -q -m "chore: stamp $NAME_SLUG from spooky-shell ($TEMPLATE_COMMIT)"

if command -v bd >/dev/null; then
  info "initializing beads (prefix: $NAME_SLUG)"
  rm -rf "$ROOT_DIR/.beads"
  if (cd "$ROOT_DIR" && bd init --prefix "$NAME_SLUG" >/dev/null 2>&1); then
    # On a corporate-managed machine bd must NOT own core.hooksPath (it would
    # shadow the managed git hooks and that tooling's daemon can write
    # corruptible binaries into .beads/hooks). Install bd's text shims into
    # .git/hooks via a TEMPORARY override, then unset it so the system-level
    # managed hooksPath stays active.
    rm -f "$ROOT_DIR/.beads/hooks"/* 2>/dev/null || true
    git -C "$ROOT_DIR" config --local --unset-all core.hooksPath 2>/dev/null || true
    git -C "$ROOT_DIR" config --local core.hooksPath .git/hooks
    (cd "$ROOT_DIR" && bd hooks install >/dev/null 2>&1) || info "WARNING: bd hooks install failed"
    git -C "$ROOT_DIR" config --local --unset core.hooksPath
    (cd "$ROOT_DIR" && \
      bd create "Replace the Note template slice with the real domain" \
        -d "Core, API, CLI, MCP, Swift UI, fixtures, and OPE contracts all carry a demo Note entity marked '// TEMPLATE SLICE'. Replace it with this project's real first entity, keeping the shape of each layer." >/dev/null 2>&1 && \
      bd create "Fill in positioning.md and the website copy" \
        -d "docs/positioning.md is a template; the website content still describes the slice. Decide audience, problem, hook." >/dev/null 2>&1 && \
      bd create "Configure release signing when ready to ship" \
        -d "Copy scripts/release.conf.example to scripts/release.conf and fill in the Apple identity. Until then builds are ad-hoc signed with SKIP_NOTARIZE." >/dev/null 2>&1 \
    ) || info "WARNING: could not seed starter beads (bd create failed); continuing"
    git -C "$ROOT_DIR" add -A
    git -C "$ROOT_DIR" commit -q -m "chore: init beads" 2>/dev/null || true
  else
    info "WARNING: bd init failed; skipping issue tracker setup"
  fi
fi

info "installing the pre-push verify gate"
"$ROOT_DIR/scripts/install-hooks.sh"

# ---------------------------------------------------------------------------
# Prove it: the stamped skeleton must build and pass on day zero.
# ---------------------------------------------------------------------------
VERIFIED_LINE="stamped and verified"
if [ "$SKIP_VERIFY" = 0 ]; then
  info "running the stamped project's own verify gate"
  "$ROOT_DIR/scripts/verify.sh" || die "the stamped project FAILED its own verify.sh — this stamp is not usable"
  info "running make build"
  (cd "$ROOT_DIR" && make build) || die "the stamped project failed to build"
else
  info "skipping verify (--skip-verify); do not ship a stamp that has not passed verify.sh"
  # Do not claim "verified" when verification was skipped.
  VERIFIED_LINE="stamped (verify SKIPPED — run ./scripts/verify.sh before trusting this stamp)"
fi

# ---------------------------------------------------------------------------
# Runway
# ---------------------------------------------------------------------------
cat <<EOF

init: done. '$NAME_HUMAN' is $VERIFIED_LINE.

Next steps:
  1. Read docs/getting-started.md (the tour) and the project guide.
  2. When ready, replace the Note slice with your domain — every file that
     carries it is marked '// TEMPLATE SLICE'. There is a bead for this.
  3. API port: $PORT (dev: $DEV_PORT). Env prefix: ${NAME_SCREAMING}_.
  4. Build-system package (root Config): $PACKAGE. If your package has a
     different name, re-stamp with --package <RealPackageName> or edit Config.
  5. When ready to ship: scripts/release.conf.example -> release.conf.

EOF
