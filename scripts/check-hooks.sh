#!/usr/bin/env sh
# Assert the local git hook baseline that keeps the hook chain (any managed
# git hooks where present, beads where in use, and the Phantom verify gate)
# intact.
#
# Environment-conditional checks:
#   - managed-hooks assertions run only when PHANTOM_MANAGED_HOOKSPATH names
#     an existing directory — on a corporate-managed machine, export it to
#     that machine's managed git-hooks path (keep it out of the repo).
#     Elsewhere the block is skipped.
#   - the beads block is required only when the repo actually uses beads
#     (a .beads/ directory exists).

phantom_lib_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/lib"
if [ ! -r "$phantom_lib_dir/hook-markers.sh" ]; then
  printf '%s\n' "phantom hooks: ERROR: missing $phantom_lib_dir/hook-markers.sh" >&2
  exit 1
fi
# shellcheck source=lib/hook-markers.sh
. "$phantom_lib_dir/hook-markers.sh"

phantom_error() {
  printf '%s\n' "phantom hooks: ERROR: $*" >&2
}

phantom_info() {
  printf '%s\n' "phantom hooks: $*"
}

phantom_fail=0

phantom_repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$phantom_repo_root" ]; then
  phantom_error "not inside a git working tree"
  exit 1
fi

phantom_git_common_dir="$(git -C "$phantom_repo_root" rev-parse --git-common-dir 2>/dev/null || true)"
if [ -z "$phantom_git_common_dir" ]; then
  phantom_error "could not resolve git common directory"
  exit 1
fi

case "$phantom_git_common_dir" in
  /*) phantom_hook_file="$phantom_git_common_dir/hooks/pre-push" ;;
  *) phantom_hook_file="$phantom_repo_root/$phantom_git_common_dir/hooks/pre-push" ;;
esac

# --------------------------------------------------------------------------
# Managed git-hooks chain (corporate-managed machines only). Set
# PHANTOM_MANAGED_HOOKSPATH in your shell profile to the machine's managed
# hooks directory to enable these checks; unset elsewhere, the block skips.
# --------------------------------------------------------------------------
PHANTOM_DEFENDER_HOOKSPATH="${PHANTOM_MANAGED_HOOKSPATH:-}"
if [ -n "$PHANTOM_DEFENDER_HOOKSPATH" ] && [ -d "$PHANTOM_DEFENDER_HOOKSPATH" ]; then
  phantom_expected_config="system	file:/etc/gitconfig	${PHANTOM_DEFENDER_HOOKSPATH}"
  phantom_hooks_path_lines="$(git -C "$phantom_repo_root" config --show-scope --show-origin --get-all core.hooksPath 2>/dev/null || true)"
  if [ "$phantom_hooks_path_lines" != "$phantom_expected_config" ]; then
    phantom_error "core.hooksPath must have exactly one system entry from file:/etc/gitconfig:"
    phantom_error "  expected: $phantom_expected_config"
    if [ -n "$phantom_hooks_path_lines" ]; then
      printf '%s\n' "$phantom_hooks_path_lines" | sed 's/^/phantom hooks:   actual: /' >&2
    else
      phantom_error "  actual: <none>"
    fi
    phantom_fail=1
  fi

  phantom_local_hooks_path="$(git -C "$phantom_repo_root" config --local --get-all core.hooksPath 2>/dev/null || true)"
  if [ -n "$phantom_local_hooks_path" ]; then
    phantom_error "repo-local core.hooksPath override shadows the managed hooks:"
    printf '%s\n' "$phantom_local_hooks_path" | sed 's/^/phantom hooks:   local: /' >&2
    phantom_fail=1
  fi

  phantom_worktree_hooks_path="$(git -C "$phantom_repo_root" config --worktree --get-all core.hooksPath 2>/dev/null || true)"
  if [ -n "$phantom_worktree_hooks_path" ]; then
    phantom_error "worktree core.hooksPath override shadows the managed hooks:"
    printf '%s\n' "$phantom_worktree_hooks_path" | sed 's/^/phantom hooks:   worktree: /' >&2
    phantom_fail=1
  fi

  phantom_defender_registered="$(git -C "$phantom_repo_root" defender is-registered 2>/dev/null || true)"
  if [ "$phantom_defender_registered" != "true" ]; then
    phantom_error "git defender is-registered did not report true"
    if [ -n "$phantom_defender_registered" ]; then
      phantom_error "  actual: $phantom_defender_registered"
    fi
    phantom_fail=1
  fi
fi

# --------------------------------------------------------------------------
# .beads/hooks must stay EMPTY: anything in there means either a bare bd
# hooks install (which shadows managed hooks) or a managed-tooling daemon
# writing corruptible binaries again. bd's real shims belong in .git/hooks.
# --------------------------------------------------------------------------
if [ -d "$phantom_repo_root/.beads/hooks" ]; then
  phantom_beads_hooks_files="$(find "$phantom_repo_root/.beads/hooks" -type f 2>/dev/null | head -5)"
  if [ -n "$phantom_beads_hooks_files" ]; then
    phantom_error ".beads/hooks contains files; it must be empty:"
    printf '%s\n' "$phantom_beads_hooks_files" | sed 's/^/phantom hooks:   /' >&2
    phantom_error "fix (playbook): rm -f .beads/hooks/*; git config --local core.hooksPath .git/hooks;"
    phantom_error "  bd hooks install; git config --local --unset core.hooksPath; ./scripts/install-hooks.sh"
    phantom_fail=1
  fi
fi

# --------------------------------------------------------------------------
# pre-push hook contents
# --------------------------------------------------------------------------
if [ ! -f "$phantom_hook_file" ]; then
  phantom_error "repo-local pre-push hook is missing: $phantom_hook_file"
  phantom_error "run ./scripts/install-hooks.sh"
  phantom_fail=1
else
  # Beads block: OPTIONAL (bd >= 1.2 does not write text blocks into
  # .git/hooks), but if one exists it must be intact — a corrupt block
  # means something ate beads-owned hook data.
  phantom_beads_status="$(phantom_beads_block_status "$phantom_hook_file")"
  case "$phantom_beads_status" in
    ok|absent)
      ;;
    mismatch)
      phantom_error "beads hook block BEGIN and END markers carry different versions in $phantom_hook_file"
      phantom_error "  BEGIN v$(phantom_beads_begin_version "$phantom_hook_file")"
      phantom_error "  END   v$(phantom_beads_end_version "$phantom_hook_file")"
      phantom_error "  both ends must agree; the block looks corrupt"
      phantom_fail=1
      ;;
    *)
      phantom_error "beads hook block is malformed in $phantom_hook_file"
      phantom_error "  found BEGIN x$(phantom_beads_begin_count "$phantom_hook_file"), END x$(phantom_beads_end_count "$phantom_hook_file"); expected exactly one of each"
      phantom_fail=1
      ;;
  esac

  if ! grep -Fqx "$PHANTOM_GATE_BEGIN" "$phantom_hook_file" || ! grep -Fqx "$PHANTOM_GATE_END" "$phantom_hook_file"; then
    phantom_error "verify gate block is missing or incomplete in $phantom_hook_file"
    phantom_error "run ./scripts/install-hooks.sh (again after bd hooks install rewrites hooks)"
    phantom_fail=1
  else
    # A marker proves only that a marker exists. The BODY between the markers
    # is what actually runs verify.sh; compare it byte-for-byte against the
    # canonical block install-hooks.sh would write today.
    phantom_script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
    if [ ! -x "$phantom_script_dir/install-hooks.sh" ]; then
      phantom_error "cannot verify the gate body: missing executable $phantom_script_dir/install-hooks.sh"
      phantom_fail=1
    else
      phantom_installed_gate="$(awk -v b="$PHANTOM_GATE_BEGIN" -v e="$PHANTOM_GATE_END" '
        $0 == b { started = 1 }
        started { print }
        started && $0 == e { exit }
      ' "$phantom_hook_file")"
      phantom_canonical_gate="$("$phantom_script_dir/install-hooks.sh" --print-gate-block)"
      if [ "$phantom_installed_gate" != "$phantom_canonical_gate" ]; then
        phantom_error "verify gate block does not match the canonical block in $phantom_hook_file"
        phantom_error "  the markers are present but the body is stale, truncated, or hand-edited —"
        phantom_error "  a marker alone proves nothing about what actually runs"
        phantom_error "  run ./scripts/install-hooks.sh to reinstall the current gate"
        phantom_fail=1
      fi
    fi
  fi
fi

if [ "$phantom_fail" -ne 0 ]; then
  exit 1
fi

phantom_info "git hook baseline OK"
