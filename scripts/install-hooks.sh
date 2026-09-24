#!/usr/bin/env sh
# Install the Phantom pre-push verify gate without clobbering other hook
# blocks (notably the beads-owned block that `bd hooks install` writes).

phantom_error() {
  printf '%s\n' "install-hooks: ERROR: $*" >&2
}

phantom_info() {
  printf '%s\n' "install-hooks: $*"
}

# Marker matchers live in one place so install-hooks.sh and check-hooks.sh
# cannot drift apart.
phantom_lib_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/lib"
if [ ! -r "$phantom_lib_dir/hook-markers.sh" ]; then
  printf '%s\n' "install-hooks: ERROR: missing $phantom_lib_dir/hook-markers.sh" >&2
  exit 1
fi
# shellcheck source=lib/hook-markers.sh
. "$phantom_lib_dir/hook-markers.sh"

# The canonical gate block, in ONE place. check-hooks.sh compares the
# installed block against this byte-for-byte (via --print-gate-block), so a
# hook whose body was emptied, truncated, or hand-edited between surviving
# markers is caught — a marker alone proves nothing.
phantom_print_gate_block() {
  cat <<'PHANTOM_BLOCK'
# --- BEGIN PHANTOM VERIFY GATE v2 ---
# This section is managed by scripts/install-hooks.sh. Do not edit by hand.
_phantom_zero_sha=0000000000000000000000000000000000000000
_phantom_ref_file="${TMPDIR:-/tmp}/phantom-pre-push.$$"
_phantom_should_verify=0

if ! cat > "$_phantom_ref_file"; then
  echo >&2 "phantom: failed to read pre-push refs from stdin"
  rm -f "$_phantom_ref_file"
  exit 1
fi

_phantom_repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$_phantom_repo_root" ]; then
  echo >&2 "phantom: could not resolve the pushing working copy"
  rm -f "$_phantom_ref_file"
  exit 1
fi

echo >&2 "phantom: checking git hook baseline"
if [ ! -x "$_phantom_repo_root/scripts/check-hooks.sh" ]; then
  echo >&2 "phantom: missing executable $_phantom_repo_root/scripts/check-hooks.sh"
  rm -f "$_phantom_ref_file"
  exit 1
fi
"$_phantom_repo_root/scripts/check-hooks.sh"
_phantom_status=$?
if [ "$_phantom_status" -ne 0 ]; then
  echo >&2 "phantom: hook baseline failed (exit $_phantom_status); push blocked before tests"
  rm -f "$_phantom_ref_file"
  exit "$_phantom_status"
fi

while read _phantom_local_ref _phantom_local_sha _phantom_remote_ref _phantom_remote_sha
do
  [ -n "$_phantom_local_ref" ] || continue
  [ "$_phantom_local_sha" = "$_phantom_zero_sha" ] && continue
  [ "$_phantom_local_sha" = "$_phantom_remote_sha" ] && continue

  case "$_phantom_remote_ref" in
    refs/tags/*)
      ;;
    *)
      _phantom_should_verify=1
      ;;
  esac
done < "$_phantom_ref_file"
# The first loop consumes the ref list; the OPE gate below needs to read it
# too, so keep a copy rather than re-reading a stdin that is already drained.
_phantom_ref_copy="${_phantom_ref_file}.ope"
cp "$_phantom_ref_file" "$_phantom_ref_copy" 2>/dev/null || _phantom_ref_copy="$_phantom_ref_file"
rm -f "$_phantom_ref_file"

if [ "$_phantom_should_verify" -ne 1 ]; then
  echo >&2 "phantom: no branch updates to verify; skipping the verify gate"
  rm -f "${_phantom_ref_copy:-}" 2>/dev/null
  unset _phantom_zero_sha _phantom_ref_file _phantom_ref_copy _phantom_should_verify _phantom_repo_root _phantom_status
else
  if [ ! -x "$_phantom_repo_root/scripts/verify.sh" ]; then
    echo >&2 "phantom: missing executable $_phantom_repo_root/scripts/verify.sh"
    echo >&2 "phantom: branch pushes fail closed when the pushing checkout cannot run the canonical verifier"
    rm -f "${_phantom_ref_copy:-}" 2>/dev/null
    unset _phantom_zero_sha _phantom_ref_file _phantom_ref_copy _phantom_should_verify _phantom_repo_root _phantom_status
    exit 1
  fi

  # OPE contract-version gate — runs only when the open-prompt-edition layer
  # is present. Cheap (a git diff), so it runs BEFORE the multi-minute
  # verifier. FAIL CLOSED when the layer exists but the gate cannot run: a
  # silently skipped checker lets a contract change through unversioned.
  if [ -d "$_phantom_repo_root/open-prompt-edition" ]; then
    if [ ! -x "$_phantom_repo_root/scripts/check-ope-version-bump.sh" ]; then
      echo >&2 "phantom: open-prompt-edition/ exists but scripts/check-ope-version-bump.sh is missing or not executable"
      echo >&2 "phantom: the OPE contract-version gate cannot run; branch pushes fail closed"
      rm -f "${_phantom_ref_copy:-}" 2>/dev/null
      exit 1
    fi
    while read _phantom_l_ref _phantom_l_sha _phantom_r_ref _phantom_r_sha
    do
      [ -n "$_phantom_l_ref" ] || continue
      [ "$_phantom_l_sha" = "$_phantom_zero_sha" ] && continue
      case "$_phantom_r_ref" in refs/tags/*) continue ;; esac
      _phantom_base="$_phantom_r_sha"
      if [ "$_phantom_base" = "$_phantom_zero_sha" ]; then
        _phantom_base="$(git -C "$_phantom_repo_root" rev-parse --verify --quiet origin/main || true)"
      fi
      if [ -z "$_phantom_base" ]; then
        # First push of a brand-new repository: no remote base can exist yet
        # (the remote ref is the zero sha and origin/main is unborn), so gate
        # the ENTIRE pushed history against its own root commit. That is a
        # stricter check than a remote diff, never a skipped one — an initial
        # push cannot smuggle an unversioned contract *change* past a base
        # that predates every commit being pushed.
        _phantom_base="$(git -C "$_phantom_repo_root" rev-list --max-parents=0 "$_phantom_l_sha" 2>/dev/null | tail -n 1)"
      fi
      if [ -z "$_phantom_base" ]; then
        echo >&2 "phantom: cannot resolve a base commit for the OPE contract-version gate (new branch and origin/main is missing)"
        echo >&2 "phantom: the gate cannot run; branch pushes fail closed. Fix: git fetch origin main"
        rm -f "${_phantom_ref_copy:-}" 2>/dev/null
        exit 1
      fi
      if ! "$_phantom_repo_root/scripts/check-ope-version-bump.sh" "$_phantom_base" "$_phantom_l_sha"; then
        echo >&2 "phantom: push blocked by the OPE contract-version gate"
        rm -f "${_phantom_ref_copy:-}" 2>/dev/null
        exit 1
      fi
    done < "$_phantom_ref_copy"
  fi

  echo >&2 "phantom: running pre-push verify gate: ./scripts/verify.sh"
  "$_phantom_repo_root/scripts/verify.sh"
  _phantom_status=$?
  if [ "$_phantom_status" -eq 0 ]; then
    echo >&2 "phantom: pre-push verify passed"
  elif [ "$_phantom_status" -eq 75 ]; then
    echo >&2 "phantom: verify lock collision (exit 75); another verify.sh is already running, so this is not a test failure"
    echo >&2 "phantom: push blocked. Wait for the active verifier to finish, then push again."
    exit 75
  else
    echo >&2 "phantom: pre-push verify failed (exit $_phantom_status); see the PASS/FAIL summary above"
    echo >&2 "phantom: push blocked. Fix the failing suite, then push again."
    exit "$_phantom_status"
  fi
  rm -f "${_phantom_ref_copy:-}" 2>/dev/null
  unset _phantom_zero_sha _phantom_ref_file _phantom_ref_copy _phantom_should_verify _phantom_repo_root _phantom_status
fi
# --- END PHANTOM VERIFY GATE v2 ---
PHANTOM_BLOCK
}

# --print-gate-block: emit the canonical gate block and exit. check-hooks.sh
# uses this to detect a stale or hand-edited installed block.
if [ "${1:-}" = "--print-gate-block" ]; then
  phantom_print_gate_block
  exit 0
fi

# Verify a CANDIDATE hook file ($1) before it is allowed to become the live
# hook. This deliberately runs against the temporary file, never the live
# one: replace-then-check leaves a corrupt ACTIVE push gate behind on any
# failure. Fail here and the live hook is still untouched.
phantom_assert_candidate() {
  phantom_candidate="$1"
  phantom_assert_failed=0
  phantom_gate_begin_count="$(phantom_count_marker "$PHANTOM_GATE_BEGIN" "$phantom_candidate")"
  phantom_gate_end_count="$(phantom_count_marker "$PHANTOM_GATE_END" "$phantom_candidate")"

  if [ "$phantom_gate_begin_count" != "1" ]; then
    phantom_error "expected exactly one gate BEGIN marker in the new hook; found $phantom_gate_begin_count"
    phantom_assert_failed=1
  fi
  if [ "$phantom_gate_end_count" != "1" ]; then
    phantom_error "expected exactly one gate END marker in the new hook; found $phantom_gate_end_count"
    phantom_assert_failed=1
  fi

  if [ "$phantom_had_beads_block" = "1" ]; then
    # Compare the BYTES of the beads block, not the marker counts. Counts
    # stay 1/1 while a strip pass deletes lines between surviving markers.
    phantom_beads_after_bytes="$(phantom_beads_block_bytes "$phantom_candidate")"
    if [ "$phantom_beads_after_bytes" != "$phantom_beads_before_bytes" ]; then
      phantom_error "beads-owned block was modified while installing the gate"
      phantom_error "  the beads block must pass through byte-for-byte; it did not"
      phantom_error "  refusing to install; your existing hook is left unchanged"
      phantom_assert_failed=1
    fi
  fi

  phantom_hook_mode="$(ls -ld "$phantom_candidate" 2>/dev/null | awk '{print $1}' | cut -c1-10)"
  if [ "$phantom_hook_mode" != "-rwxr-xr-x" ]; then
    phantom_error "expected the new hook to have mode 755 (-rwxr-xr-x); found ${phantom_hook_mode:-<missing>}"
    phantom_assert_failed=1
  fi

  if [ "$phantom_assert_failed" -ne 0 ]; then
    return 1
  fi
  return 0
}

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
  /*) phantom_hooks_dir="$phantom_git_common_dir/hooks" ;;
  *) phantom_hooks_dir="$phantom_repo_root/$phantom_git_common_dir/hooks" ;;
esac

phantom_hook_file="$phantom_hooks_dir/pre-push"
mkdir -p "$phantom_hooks_dir" || exit 1

# Repair: bd >= 1.2 points local/worktree core.hooksPath at .beads/hooks
# (binary hook shims). That override shadows managed git hooks on a
# corporate-managed machine and the shims can fail with "exec format
# error", blocking every push. The verify gate lives in .git/hooks, so the
# override must go. beads itself still syncs fine without its git hooks
# (bd dolt push/pull and the daemon do the work).
for phantom_scope in --local --worktree; do
  phantom_hp="$(git -C "$phantom_repo_root" config "$phantom_scope" --get core.hooksPath 2>/dev/null || true)"
  case "$phantom_hp" in
    */.beads/hooks)
      phantom_info "removing $phantom_scope core.hooksPath override ($phantom_hp)"
      git -C "$phantom_repo_root" config "$phantom_scope" --unset-all core.hooksPath || exit 1
      ;;
  esac
done

if [ ! -w "$phantom_hooks_dir" ] || [ ! -x "$phantom_hooks_dir" ]; then
  phantom_error "hooks directory is not writable/searchable: $phantom_hooks_dir"
  exit 1
fi

if [ -e "$phantom_hook_file" ] && [ ! -w "$phantom_hook_file" ]; then
  phantom_error "existing pre-push hook is not writable: $phantom_hook_file"
  exit 1
fi

phantom_had_beads_block=0
phantom_beads_before_bytes=""
if [ -f "$phantom_hook_file" ]; then
  phantom_beads_status="$(phantom_beads_block_status "$phantom_hook_file")"
  case "$phantom_beads_status" in
    ok)
      phantom_had_beads_block=1
      phantom_beads_before_bytes="$(phantom_beads_block_bytes "$phantom_hook_file")"
      phantom_info "preserving beads-owned block ($(phantom_beads_version_label "$phantom_hook_file"))"
      ;;
    mismatch)
      phantom_error "beads hook block markers disagree in $phantom_hook_file"
      phantom_error "  BEGIN says v$(phantom_beads_begin_version "$phantom_hook_file")"
      phantom_error "  END says   v$(phantom_beads_end_version "$phantom_hook_file")"
      phantom_error "refusing to install: the beads block looks corrupt, and rewriting the"
      phantom_error "hook could destroy beads-owned data. Repair it (or re-run bd hooks"
      phantom_error "install), then rerun ./scripts/install-hooks.sh"
      exit 1
      ;;
    malformed)
      phantom_error "beads hook block is malformed in $phantom_hook_file"
      phantom_error "  found BEGIN x$(phantom_beads_begin_count "$phantom_hook_file"), END x$(phantom_beads_end_count "$phantom_hook_file"); expected exactly one of each"
      phantom_error "refusing to install: repair the beads block (or re-run bd hooks install),"
      phantom_error "then rerun ./scripts/install-hooks.sh"
      exit 1
      ;;
  esac
fi

phantom_tmp_file="$(mktemp "$phantom_hooks_dir/pre-push.phantom.XXXXXX")" || exit 1
phantom_stripped_file="$(mktemp "$phantom_hooks_dir/pre-push.phantom-stripped.XXXXXX")" || {
  rm -f "$phantom_tmp_file"
  exit 1
}
phantom_block_file="$(mktemp "$phantom_hooks_dir/pre-push.phantom-block.XXXXXX")" || {
  rm -f "$phantom_tmp_file" "$phantom_stripped_file"
  exit 1
}

phantom_cleanup() {
  rm -f "$phantom_tmp_file" "$phantom_stripped_file" "$phantom_block_file"
}

trap phantom_cleanup EXIT HUP INT TERM

phantom_print_gate_block > "$phantom_block_file" || exit 1

if [ -f "$phantom_hook_file" ]; then
  phantom_backup="$phantom_hook_file.phantom-backup-$(date +%Y%m%d%H%M%S).$$"
  command cp -p "$phantom_hook_file" "$phantom_backup" || exit 1
  phantom_info "backed up existing hook to $phantom_backup"

  if ! awk '
    /^# --- BEGIN PHANTOM VERIFY GATE / {
      if (in_gate_block) {
        print "install-hooks: ERROR: nested verify gate BEGIN marker" > "/dev/stderr"
        exit 2
      }
      in_gate_block = 1
      next
    }
    /^# --- END PHANTOM VERIFY GATE / {
      if (!in_gate_block) {
        print "install-hooks: ERROR: verify gate END marker without BEGIN" > "/dev/stderr"
        exit 2
      }
      in_gate_block = 0
      next
    }
    {
      if (!in_gate_block) {
        print
      }
    }
    END {
      if (in_gate_block) {
        print "install-hooks: ERROR: verify gate BEGIN marker without END; refusing to rewrite hook" > "/dev/stderr"
        exit 2
      }
    }
  ' "$phantom_hook_file" > "$phantom_stripped_file"; then
    phantom_error "hook left unchanged"
    exit 1
  fi
else
  printf '%s\n' '#!/usr/bin/env sh' > "$phantom_stripped_file" || exit 1
fi

cat "$phantom_stripped_file" "$phantom_block_file" > "$phantom_tmp_file" || exit 1
command chmod 755 "$phantom_tmp_file" || exit 1

# Verify the candidate BEFORE it becomes the live hook. mv-then-check leaves
# the user's ACTIVE pre-push hook corrupt on any assertion failure. A tool
# whose failure mode is "your push gate is now broken" is worse than the bug
# it was written to fix.
if ! phantom_assert_candidate "$phantom_tmp_file"; then
  phantom_error "refusing to install the new hook; verification of the candidate failed"
  if [ -n "${phantom_backup:-}" ]; then
    phantom_error "your existing hook at $phantom_hook_file is UNCHANGED (backup: $phantom_backup)"
  else
    phantom_error "no hook was installed at $phantom_hook_file"
  fi
  exit 1
fi

command mv -f "$phantom_tmp_file" "$phantom_hook_file" || exit 1

# Belt and braces: re-assert against the live file (a concurrent writer could
# have raced the mv) and roll back to the backup if it does not hold.
if ! phantom_assert_candidate "$phantom_hook_file"; then
  phantom_error "post-install verification failed against the live hook"
  if [ -n "${phantom_backup:-}" ] && [ -f "$phantom_backup" ]; then
    if command cp -p "$phantom_backup" "$phantom_hook_file"; then
      phantom_error "restored your previous hook from $phantom_backup"
    else
      phantom_error "FAILED to restore from $phantom_backup -- restore it by hand:"
      phantom_error "  cp -p $phantom_backup $phantom_hook_file"
    fi
  else
    phantom_error "there was no previous hook to restore; removing the incomplete hook"
    rm -f "$phantom_hook_file"
  fi
  exit 1
fi

trap - EXIT HUP INT TERM
phantom_cleanup

phantom_info "installed verify gate in $phantom_hook_file"
