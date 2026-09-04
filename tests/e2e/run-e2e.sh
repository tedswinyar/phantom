#!/usr/bin/env bash
set -euo pipefail

# run-e2e.sh — the parity harness: two-implementations-one-database as a
# day-zero gate. A scan recorded through the CLI must read back byte-identical
# (after key-sorting) through raw HTTP, the CLI, and the MCP server. This
# is the test shape that catches wire-format drift no unit suite can see.
#
# The scan sections walk a DETERMINISTIC fixture tree: fixed contents, files
# on both sides of the 1 MiB persistence boundary (ADR-0005), a compressed
# file whose diskSize and logicalSize diverge (the seam v0.1 got wrong), and
# `touch -t` mtimes so even datetimes are pinned end-to-end. Machine-variable
# fields (ids, timestamps, host paths) are never compared against constants —
# except the mtime we set ourselves.
#
# Requires debug binaries (cargo build --workspace); verify.sh runs the
# rust suite first, so they exist by the time this runs under verify.

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT_DIR/rust/target/debug"
WORK="$(mktemp -d /tmp/phantom-e2e.XXXXXX)"
API_PID=""

fail() {
  echo "e2e: FAIL: $*" >&2
  exit 1
}

cleanup() {
  if [ -n "$API_PID" ]; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# ALWAYS build. `cargo test` compiles test harnesses, not the shipping
# binaries, and an existing binary can be STALE — a stale green binary
# passing e2e while current source is broken is the failure mode that
# matters (bit a stamped project during mutation testing, 2026-08-19).
# A no-op build is nearly free.
echo "e2e: building workspace binaries..."
(cd "$ROOT_DIR/rust" && cargo build --workspace --quiet) || fail "cargo build failed"
for bin in phantom-api phantom phantom-mcp; do
  [ -x "$BIN/$bin" ] || fail "missing $BIN/$bin after cargo build"
done
command -v jq >/dev/null || fail "jq is required"

# One MCP tool call per process: print the JSON-RPC response for a tool.
mcp_call() { # name, arguments-json
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' \
    "$1" "$2" | "$BIN/phantom-mcp"
}

# The tool's result payload (JSON text content), failing if isError is set.
mcp_result() { # name, arguments-json
  local resp
  resp="$(mcp_call "$1" "$2")"
  echo "$resp" | jq -e '.result.isError != true' >/dev/null \
    || fail "MCP $1 errored: $(echo "$resp" | jq -r '.result.content[0].text')"
  echo "$resp" | jq -r '.result.content[0].text'
}

# The tool's ERROR text, failing if the call unexpectedly succeeded.
mcp_error_text() { # name, arguments-json
  local resp
  resp="$(mcp_call "$1" "$2")"
  echo "$resp" | jq -e '.result.isError == true' >/dev/null \
    || fail "MCP $1 should have errored, got: $resp"
  echo "$resp" | jq -r '.result.content[0].text'
}

# ---------------------------------------------------------------------------
# Boot the API in the test profile on an ephemeral port
# ---------------------------------------------------------------------------
export PHANTOM_PROFILE=test
export PHANTOM_DB_PATH="$WORK/phantom.db"
export PHANTOM_PORT=0
export PHANTOM_KEY_FILE="$WORK/api_key"

"$BIN/phantom-api" >"$WORK/api.out" 2>"$WORK/api.err" &
API_PID=$!

BASE=""
for _ in $(seq 1 50); do
  BASE="$(sed -n 's/.*listening on //p' "$WORK/api.out")"
  [ -n "$BASE" ] && break
  kill -0 "$API_PID" 2>/dev/null || { cat "$WORK/api.err" >&2; fail "API died on startup"; }
  sleep 0.1
done
[ -n "$BASE" ] || fail "API never announced its port"
export PHANTOM_API_URL="$BASE"
API_KEY="$(cat "$WORK/api_key")"
http_get() { curl -sf -H "x-api-key: $API_KEY" "$@"; }
echo "e2e: API at $BASE"

# ---------------------------------------------------------------------------
# 1. Auth boundary: no key → 401; /health open
# ---------------------------------------------------------------------------
STATUS="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/scans")"
[ "$STATUS" = "401" ] || fail "unauthenticated /scans returned $STATUS, want 401"
curl -sf "$BASE/health" | jq -e '.status == "ok"' >/dev/null || fail "/health not ok"

# ---------------------------------------------------------------------------
# 2. Capability parity: every operation exists on ALL THREE surfaces
# (agentapi C2). Each surface hand-maintains its own list; without this gate
# they drift silently — health already shipped on HTTP+CLI but not MCP once.
# Adding a route without its MCP tool + CLI subcommand fails this check.
# ---------------------------------------------------------------------------
EXPECTED_TOOLS="$(printf '%s\n' diff_scans find_large_files get_hotspots \
  get_space_by_type get_treemap health list_scans scan_directory)"
MCP_TOOLS="$(printf '%s\n' '{"jsonrpc":"2.0","id":9,"method":"tools/list"}' \
  | "$BIN/phantom-mcp" | jq -r '.result.tools[].name' | sort)"
[ "$MCP_TOOLS" = "$EXPECTED_TOOLS" ] || fail "MCP tool set drifted from the parity table:
  want: $(echo "$EXPECTED_TOOLS" | tr '\n' ' ')
  got:  $(echo "$MCP_TOOLS" | tr '\n' ' ')"

# Every operation must also be a reachable CLI subcommand (--help exits 0).
for cli in "scan" "scans list" "scans show" "scans cancel" "scans delete" \
  "top" "tree" "types" "hotspots" "diff" "health"; do
  # shellcheck disable=SC2086
  "$BIN/phantom" $cli --help >/dev/null 2>&1 \
    || fail "CLI subcommand missing for parity: '$cli'"
done

# The specific gap C2 warns about: health must be reachable on all three.
"$BIN/phantom" health | jq -e '.status == "ok"' >/dev/null \
  || fail "health via CLI not ok"
mcp_result health '{}' | jq -e '.status == "ok"' >/dev/null \
  || fail "health via MCP not ok"
echo "e2e: capability parity holds across HTTP, CLI, MCP"

# ---------------------------------------------------------------------------
# 3. Error-shape contract on the agent-prone paths (agentapi C1): a bad UUID
# and an unknown field must return {error}-shaped JSON, and the CLI/MCP must
# surface the REAL reason (never "invalid JSON from API"). Mutation-proof:
# revert the wrapper extractors and the bodies become text/plain → the MCP
# error text stops mentioning the real cause.
# ---------------------------------------------------------------------------
curl -s -H "x-api-key: $API_KEY" "$BASE/scans/not-a-uuid" | jq -e 'has("error")' >/dev/null \
  || fail "malformed uuid did not return {error} shape"
MCP_ERRTEXT="$(mcp_error_text get_space_by_type '{"scanId":"not-a-uuid"}')"
printf '%s' "$MCP_ERRTEXT" | grep -qi 'uuid' \
  || fail "MCP swallowed the real bad-uuid error; got: $MCP_ERRTEXT"
echo "e2e: error-shape contract holds through MCP"

# ---------------------------------------------------------------------------
# 4. Scan domain — no-scans-yet error paths, then the deterministic tree
# ---------------------------------------------------------------------------
# Before any scan exists, defaulting tools/commands must say so plainly.
mcp_error_text find_large_files '{}' | grep -q 'no completed scans' \
  || fail "find_large_files without scans must name the real problem"
rc=0; "$BIN/phantom" types >/dev/null 2>"$WORK/types.err" || rc=$?
[ "$rc" = 3 ] || fail "CLI types with no scans: want exit 3, got $rc"
grep -q 'no completed scans' "$WORK/types.err" \
  || fail "CLI types must explain there are no scans"

MIB=1048576
TREE="$WORK/haunt"
mkdir -p "$TREE/sub" "$TREE/empty"
perl -e 'print "\x07" x (1024*1024)'   > "$TREE/big.bin"        # exactly 1 MiB — ON the inclusive boundary
printf 'tiny bytes'                    > "$TREE/small.txt"      # 10 B — row filtered, bytes still counted
perl -e 'print "\x09" x (2*1024*1024)' > "$TREE/sub/medium.log" # 2 MiB
perl -e 'print "\x01" x 100'           > "$TREE/sub/tiny.rs"    # 100 B — row filtered
# cloud.dat: 10 MiB logical, ~2 MiB on disk (2 MiB of seeded-PRNG
# incompressible bytes + 8 MiB of constant, decmpfs-compressed via ditto —
# plain APFS writes never leave holes here, verified 2026-08-31). diskSize
# and logicalSize DIVERGE, the dataloaded-cloud-file shape — any surface
# reporting logical as the headline number fails the checks below (v0.1's
# CLI and MCP did exactly that).
perl -e 'srand(42); print map { chr(int(rand 256)) } 1..(2*1024*1024); print "\x05" x (8*1024*1024)' \
  > "$WORK/cloud.raw"
ditto --hfsCompression "$WORK/cloud.raw" "$TREE/cloud.dat"
rm "$WORK/cloud.raw"
# Pin mtimes so datetimes are deterministic end-to-end (touch -t reads local
# time; force UTC so the wire value is a constant).
find "$TREE" -exec env TZ=UTC touch -t 202601011200.00 {} +

SCAN_JSON="$("$BIN/phantom" scan "$TREE" --json)"
SCAN_ID="$(echo "$SCAN_JSON" | jq -r .id)"
[ -n "$SCAN_ID" ] && [ "$SCAN_ID" != "null" ] || fail "CLI scan returned no id"
[ "$(echo "$SCAN_JSON" | jq -r .status)" = "complete" ] \
  || fail "CLI scan --json must wait for completion: $SCAN_JSON"
echo "$SCAN_JSON" | jq -e '.fileCount == 5 and .dirCount == 3 and .errorCount == 0' >/dev/null \
  || fail "scan totals wrong (want 5 files, 3 dirs): $SCAN_JSON"
echo "$SCAN_JSON" | jq -e '.progress == null' >/dev/null \
  || fail "terminal scan must carry progress: null"
echo "e2e: scanned the fixture tree as $SCAN_ID"

# ---------------------------------------------------------------------------
# 5. Three-view byte parity on the same scan (key-sorted)
# ---------------------------------------------------------------------------
SCAN_CLI="$("$BIN/phantom" scans show "$SCAN_ID" --json | jq -Sc .)"
SCAN_HTTP="$(http_get "$BASE/scans/$SCAN_ID" | jq -Sc .)"
SCAN_MCP="$(mcp_result list_scans '{}' \
  | jq -Sc --arg id "$SCAN_ID" '.[] | select(.id == $id)')"

[ "$SCAN_CLI" = "$SCAN_HTTP" ] || fail "scan: CLI and HTTP disagree:
  cli:  $SCAN_CLI
  http: $SCAN_HTTP"
[ "$SCAN_HTTP" = "$SCAN_MCP" ] || fail "scan: HTTP and MCP disagree:
  http: $SCAN_HTTP
  mcp:  $SCAN_MCP"

# Canonical datetime on the wire (6 fractional digits, Z form).
FINISHED="$(echo "$SCAN_HTTP" | jq -r .finishedAt)"
case "$FINISHED" in
  *.??????Z) : ;;
  *) fail "finishedAt not canonical 6-digit format: $FINISHED" ;;
esac
echo "e2e: three views agree on scan $SCAN_ID"

# ---------------------------------------------------------------------------
# 6. Files: parity, the 1 MiB boundary, and diskSize vs logicalSize
# ---------------------------------------------------------------------------
FILES_HTTP="$(http_get "$BASE/scans/$SCAN_ID/files" | jq -Sc .)"
FILES_CLI="$("$BIN/phantom" top --scan "$SCAN_ID" --json | jq -Sc .)"
FILES_MCP="$(mcp_result find_large_files "{\"scanId\":\"$SCAN_ID\"}" | jq -Sc .files)"

[ "$FILES_CLI" = "$FILES_HTTP" ] || fail "files: CLI and HTTP disagree:
  cli:  $FILES_CLI
  http: $FILES_HTTP"
[ "$FILES_HTTP" = "$FILES_MCP" ] || fail "files: HTTP and MCP disagree:
  http: $FILES_HTTP
  mcp:  $FILES_MCP"

# The ADR-0005 boundary, observed over the wire: big.bin (exactly 1 MiB) is
# kept; small.txt and tiny.rs are not. Names are host-path-independent.
echo "$FILES_HTTP" | jq -e '[.[].name] | sort == ["big.bin","cloud.dat","medium.log"]' >/dev/null \
  || fail "persisted file set wrong: $(echo "$FILES_HTTP" | jq -c '[.[].name]')"
echo "$FILES_HTTP" | jq -e '.[-1].name == "big.bin"' >/dev/null \
  || fail "size-descending order: 1 MiB big.bin must be last"

# The seam that mattered: cloud.dat's diskSize is ~2 MiB while its
# logicalSize is exactly 10 MiB. diskSize is THE size.
echo "$FILES_HTTP" | jq -e --argjson mib "$MIB" '
  [.[] | select(.name == "cloud.dat")][0] |
  .logicalSize == 10 * $mib and .diskSize >= 2 * $mib and .diskSize < 4 * $mib
' >/dev/null || fail "cloud.dat disk/logical divergence not recorded: \
$(echo "$FILES_HTTP" | jq -c '.[] | select(.name == "cloud.dat")')"

# Human output formats the DISK size (single-digit MB — decmpfs allocation
# varies a little run to run), never the logical (10.0 MB).
TOP_HUMAN="$("$BIN/phantom" top --scan "$SCAN_ID")"
echo "$TOP_HUMAN" | grep -F 'cloud.dat' | grep -Eq '^[0-9]\.[0-9] MB' \
  || fail "human top must show cloud.dat's disk size: $TOP_HUMAN"
echo "$TOP_HUMAN" | grep -qF '10.0 MB' \
  && fail "human top leaked a logical size as the headline: $TOP_HUMAN"

# The pinned mtime survives the whole pipeline in canonical wire form.
BIG_ENTRY="$(http_get "$BASE/scans/$SCAN_ID/entry?path=$TREE/big.bin")"
[ "$(echo "$BIG_ENTRY" | jq -r .modifiedAt)" = "2026-01-01T12:00:00.000000Z" ] \
  || fail "pinned mtime drifted: $(echo "$BIG_ENTRY" | jq -r .modifiedAt)"

# Path-sorted listing is deterministic across the parity surfaces too.
http_get "$BASE/scans/$SCAN_ID/files?sort=path" \
  | jq -e '[.[].name] == ["big.bin","cloud.dat","medium.log"]' >/dev/null \
  || fail "sort=path listing not path-ordered"

# Pagination rides X-Next-Cursor everywhere: MCP surfaces it inline, the CLI
# as a stderr hint.
mcp_result find_large_files "{\"scanId\":\"$SCAN_ID\",\"limit\":1}" \
  | jq -e '(.files | length == 1) and (.nextCursor != null)' >/dev/null \
  || fail "MCP find_large_files must surface the continuation cursor"
"$BIN/phantom" top --scan "$SCAN_ID" --limit 1 --json >/dev/null 2>"$WORK/top.err"
grep -q -- '--cursor' "$WORK/top.err" || fail "CLI top must hint at the continuation cursor"
echo "e2e: files parity, 1 MiB boundary, and disk-vs-logical hold"

# ---------------------------------------------------------------------------
# 7. Types: parity + computed from the FULL walk (filtered rows count)
# ---------------------------------------------------------------------------
TYPES_HTTP="$(http_get "$BASE/scans/$SCAN_ID/types" | jq -Sc .)"
TYPES_CLI="$("$BIN/phantom" types --scan "$SCAN_ID" --json | jq -Sc .)"
TYPES_MCP="$(mcp_result get_space_by_type "{\"scanId\":\"$SCAN_ID\"}" | jq -Sc .)"

[ "$TYPES_CLI" = "$TYPES_HTTP" ] || fail "types: CLI and HTTP disagree:
  cli:  $TYPES_CLI
  http: $TYPES_HTTP"
[ "$TYPES_HTTP" = "$TYPES_MCP" ] || fail "types: HTTP and MCP disagree:
  http: $TYPES_HTTP
  mcp:  $TYPES_MCP"

# Disk-desc order with name tiebreak; txt and rs prove the totals saw the
# FULL walk (their file rows were filtered at persistence).
echo "$TYPES_HTTP" | jq -e '[.[].fileType] == ["dat","log","bin","rs","txt"]' >/dev/null \
  || fail "type totals order/content wrong: $TYPES_HTTP"
"$BIN/phantom" types --scan "$SCAN_ID" | head -1 | grep -qF '.dat' \
  || fail "human types must lead with the largest type"
echo "e2e: type totals agree and see the full walk"

# ---------------------------------------------------------------------------
# 8. Tree: parity at depth 1, and --depth actually descends (the v0.1 fix)
# ---------------------------------------------------------------------------
TREE_HTTP="$(http_get "$BASE/scans/$SCAN_ID/tree" | jq -Sc .)"
TREE_CLI="$("$BIN/phantom" tree --scan "$SCAN_ID" --depth 1 --json | jq -Sc .)"
[ "$TREE_CLI" = "$TREE_HTTP" ] || fail "tree: CLI depth-1 and HTTP disagree:
  cli:  $TREE_CLI
  http: $TREE_HTTP"

echo "$TREE_HTTP" | jq -e '[.[].name] == ["big.bin","cloud.dat","empty","sub"]' >/dev/null \
  || fail "depth-1 tree wrong (path-ordered direct children): $TREE_HTTP"

# THE regression pin: v1.0's --depth must change the output (v0.1 ignored it).
"$BIN/phantom" tree --scan "$SCAN_ID" --depth 1 --json | jq -e '[.[].name] | index("medium.log") == null' >/dev/null \
  || fail "--depth 1 must not descend into sub/"
"$BIN/phantom" tree --scan "$SCAN_ID" --depth 2 --json | jq -e '[.[].name] | index("medium.log") != null' >/dev/null \
  || fail "--depth 2 must include sub/medium.log — the ignored-depth bug is back"
"$BIN/phantom" tree --scan "$SCAN_ID" --depth 2 | grep -qF 'medium.log' \
  || fail "human tree at depth 2 must render sub/medium.log"

# Directory aggregation over the wire: sub carries medium.log AND filtered
# tiny.rs bytes; the scan root's aggregate equals the scan's own total.
echo "$TREE_HTTP" | jq -e --argjson mib "$MIB" '
  [.[] | select(.name == "sub")][0].diskSize > 2 * $mib
' >/dev/null || fail "sub/ aggregate must include its filtered small file"
ROOT_ENTRY="$(http_get "$BASE/scans/$SCAN_ID/entry?path=$TREE")"
[ "$(echo "$ROOT_ENTRY" | jq .diskSize)" = "$(echo "$SCAN_JSON" | jq .totalDiskSize)" ] \
  || fail "root aggregate != scan total"
echo "e2e: tree parity and --depth hold"

# ---------------------------------------------------------------------------
# 9. Treemap: HTTP↔MCP parity, laid out at the requested size, re-rooted
# ---------------------------------------------------------------------------
# Layout floats are semantically identical across surfaces but their decimal
# RENDERING is not byte-stable through a parse→re-serialize cycle (observed:
# …614 vs …617 in the 17th digit for the same double). Normalize numbers to
# micro-precision before comparing; everything else stays byte-exact.
norm_floats() { jq -Sc 'walk(if type == "number" then (. * 1e6 | round) else . end)'; }

MAP_HTTP="$(http_get "$BASE/scans/$SCAN_ID/treemap?width=400&height=300&maxDepth=2" | norm_floats)"
MAP_MCP="$(mcp_result get_treemap \
  "{\"scanId\":\"$SCAN_ID\",\"width\":400,\"height\":300,\"maxDepth\":2}" | norm_floats)"
[ "$MAP_HTTP" = "$MAP_MCP" ] || fail "treemap: HTTP and MCP disagree:
  http: $MAP_HTTP
  mcp:  $MAP_MCP"
echo "$MAP_HTTP" | jq -e '.rects[0].width == 400000000 and .rects[0].height == 300000000' >/dev/null \
  || fail "treemap not laid out at the requested view size"

SUBMAP_HTTP="$(http_get "$BASE/scans/$SCAN_ID/treemap?root=$TREE/sub" | norm_floats)"
SUBMAP_MCP="$(mcp_result get_treemap "{\"scanId\":\"$SCAN_ID\",\"root\":\"$TREE/sub\"}" | norm_floats)"
[ "$SUBMAP_HTTP" = "$SUBMAP_MCP" ] || fail "re-rooted treemap: HTTP and MCP disagree"
echo "$SUBMAP_HTTP" | jq -e --arg sub "$TREE/sub" '.rootPath == $sub' >/dev/null \
  || fail "treemap root= did not re-root"
echo "e2e: treemap parity holds"

# ---------------------------------------------------------------------------
# 10. Lifecycle through every surface: MCP scan_directory (wait and no-wait),
# CLI --no-wait, cancel/delete semantics, and the exit-code contract
# ---------------------------------------------------------------------------
# MCP scan_directory (default wait) returns a COMPLETED scan.
SCAN2_JSON="$(mcp_result scan_directory "{\"path\":\"$TREE\"}")"
SCAN2_ID="$(echo "$SCAN2_JSON" | jq -r .id)"
[ "$(echo "$SCAN2_JSON" | jq -r .status)" = "complete" ] \
  || fail "scan_directory must wait to completion by default: $SCAN2_JSON"
echo "$SCAN2_JSON" | jq -e '.fileCount == 5' >/dev/null \
  || fail "MCP rescan saw a different tree: $SCAN2_JSON"

"$BIN/phantom" scans list --json | jq -e 'length == 2' >/dev/null \
  || fail "scans list must show both scans"

# wait:false answers immediately with the RUNNING view.
SCAN3_JSON="$(mcp_result scan_directory "{\"path\":\"$TREE\",\"wait\":false}")"
SCAN3_ID="$(echo "$SCAN3_JSON" | jq -r .id)"
[ "$(echo "$SCAN3_JSON" | jq -r .status)" = "running" ] \
  || fail "scan_directory wait:false must return the running scan"
echo "$SCAN3_JSON" | jq -e '.progress | type == "object"' >/dev/null \
  || fail "running scan must carry a progress object"

# CLI --no-wait: same contract, and `scans show` polls it to terminal.
SCAN4_ID="$("$BIN/phantom" scan "$TREE" --no-wait --json | jq -r .id)"
for waiting in "$SCAN3_ID" "$SCAN4_ID"; do
  for _ in $(seq 1 100); do
    STATUS="$("$BIN/phantom" scans show "$waiting" --json | jq -r .status)"
    [ "$STATUS" != "running" ] && break
    sleep 0.1
  done
  [ "$STATUS" = "complete" ] || fail "background scan $waiting ended $STATUS"
done

# Cancelling a terminal scan is a server-rejected request: exit 1.
rc=0; "$BIN/phantom" scans cancel "$SCAN2_ID" >/dev/null 2>"$WORK/cancel.err" || rc=$?
[ "$rc" = 1 ] || fail "cancel of a terminal scan: want exit 1, got $rc"
grep -q 'cannot cancel' "$WORK/cancel.err" || fail "cancel error must carry the reason"

# Delete the extra scans; deleting again (or showing them) is a clean 404 → 3.
for gone in "$SCAN2_ID" "$SCAN3_ID" "$SCAN4_ID"; do
  "$BIN/phantom" scans delete "$gone" >/dev/null || fail "delete $gone failed"
done
rc=0; "$BIN/phantom" scans show "$SCAN2_ID" >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || fail "show of a deleted scan: want exit 3, got $rc"
rc=0; "$BIN/phantom" scans delete "$SCAN2_ID" >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || fail "double delete: want exit 3, got $rc"
"$BIN/phantom" scans list --json | jq -e 'length == 1' >/dev/null \
  || fail "only the original scan should remain"

# Deleted scans are gone from the MCP view too (store-backed, not cached).
mcp_error_text get_space_by_type "{\"scanId\":\"$SCAN2_ID\"}" | grep -qi 'not found' \
  || fail "MCP must surface not-found for a deleted scan"

# Bad request via CLI: scanning a non-directory is a 400 → exit 1.
rc=0; "$BIN/phantom" scan /no/such/dir/anywhere >/dev/null 2>"$WORK/scan.err" || rc=$?
[ "$rc" = 1 ] || fail "scan of a bad path: want exit 1, got $rc"
grep -q 'not a directory' "$WORK/scan.err" || fail "scan error must carry the real reason"

# Usage error → 2 (clap's contract); unreachable API → 4.
rc=0; "$BIN/phantom" scans >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] || fail "usage error: want exit 2, got $rc"
rc=0; "$BIN/phantom" --api-url http://127.0.0.1:9 health >/dev/null 2>&1 || rc=$?
[ "$rc" = 4 ] || fail "unreachable API: want exit 4, got $rc"
echo "e2e: lifecycle semantics and exit codes hold"

# ---------------------------------------------------------------------------
# 11. Hotspots (Phase 5): three-view byte parity, full-walk classification,
# categories persisted on entry rows (dir rows included), zero-hotspot shape
# ---------------------------------------------------------------------------
# The original fixture tree has no hotspots: the summary is honestly empty,
# and parity holds on the empty shape too.
EMPTY_HTTP="$(http_get "$BASE/scans/$SCAN_ID/hotspots" | jq -Sc .)"
EMPTY_CLI="$("$BIN/phantom" hotspots --scan "$SCAN_ID" --json | jq -Sc .)"
[ "$EMPTY_CLI" = "$EMPTY_HTTP" ] || fail "empty hotspots: CLI and HTTP disagree:
  cli:  $EMPTY_CLI
  http: $EMPTY_HTTP"
echo "$EMPTY_HTTP" | jq -e '.groups == [] and .reclaimEstimate == 0' >/dev/null \
  || fail "no-hotspot scan must serve the empty summary: $EMPTY_HTTP"
"$BIN/phantom" hotspots --scan "$SCAN_ID" | grep -qF 'no hotspots found' \
  || fail "human hotspots must say when there are none"

# Uncategorized entries carry category present-as-null, never absent.
echo "$BIG_ENTRY" | jq -e 'has("category") and .category == null' >/dev/null \
  || fail "ordinary entry must carry category: null: $BIG_ENTRY"

# A tree WITH a hotspot: node_modules holding a file on each side of the
# 1 MiB persistence boundary. No package.json marker at the root, so no
# project-root/dormancy math — the category is regenerableArtifact
# deterministically, independent of today's date.
HOT="$WORK/hotroot"
mkdir -p "$HOT/node_modules"
perl -e 'print "\x02" x (1024*1024)' > "$HOT/node_modules/chunk.bin" # 1 MiB — persisted row
printf 'tiny bytes'                  > "$HOT/node_modules/tiny.js"   # 10 B — row filtered

HOT_SCAN="$("$BIN/phantom" scan "$HOT" --json)"
HOT_ID="$(echo "$HOT_SCAN" | jq -r .id)"
[ "$(echo "$HOT_SCAN" | jq -r .status)" = "complete" ] || fail "hotspot scan did not complete"

HOT_HTTP="$(http_get "$BASE/scans/$HOT_ID/hotspots" | jq -Sc .)"
HOT_CLI="$("$BIN/phantom" hotspots --scan "$HOT_ID" --json | jq -Sc .)"
HOT_MCP="$(mcp_result get_hotspots "{\"scanId\":\"$HOT_ID\"}" | jq -Sc .)"
[ "$HOT_CLI" = "$HOT_HTTP" ] || fail "hotspots: CLI and HTTP disagree:
  cli:  $HOT_CLI
  http: $HOT_HTTP"
[ "$HOT_HTTP" = "$HOT_MCP" ] || fail "hotspots: HTTP and MCP disagree:
  http: $HOT_HTTP
  mcp:  $HOT_MCP"

# One group: node_modules, regenerable. fileCount 2 and a reclaim estimate
# STRICTLY above 1 MiB prove the classifier saw the FULL walk — tiny.js has
# no persisted row, yet its blocks are in the estimate.
echo "$HOT_HTTP" | jq -e --argjson mib "$MIB" '
  (.groups | length == 1)
  and .groups[0].ruleId == "node-modules"
  and .groups[0].category == "regenerableArtifact"
  and .groups[0].fileCount == 2
  and .reclaimEstimate > $mib and .reclaimEstimate < 2 * $mib
  and .reclaimEstimate == .groups[0].diskSize
  and .reviewDiskSize == 0
' >/dev/null || fail "hotspot summary wrong: $HOT_HTTP"
echo "$HOT_HTTP" | jq -e --arg p "$HOT/node_modules" '.groups[0].topPaths == [$p]' >/dev/null \
  || fail "topPaths must name the hotspot root: $HOT_HTTP"

# Categories persisted on entry rows — the DIRECTORY row included.
http_get "$BASE/scans/$HOT_ID/entry?path=$HOT/node_modules" \
  | jq -e '.isDir == true and .category == "regenerableArtifact"' >/dev/null \
  || fail "node_modules dir row must carry its category"
http_get "$BASE/scans/$HOT_ID/entry?path=$HOT/node_modules/chunk.bin" \
  | jq -e '.category == "regenerableArtifact"' >/dev/null \
  || fail "chunk.bin row must carry its category"

# With no --scan the CLI defaults to the latest completed scan (this one).
"$BIN/phantom" hotspots --json | jq -e '.groups[0].ruleId == "node-modules"' >/dev/null \
  || fail "hotspots must default to the latest completed scan"

# Human output: the deduped disk size headline, label, and the action hint.
HOT_HUMAN="$("$BIN/phantom" hotspots --scan "$HOT_ID")"
echo "$HOT_HUMAN" | grep -qF 'node_modules directories' \
  || fail "human hotspots must show the group label: $HOT_HUMAN"
echo "$HOT_HUMAN" | grep -qF 'hint: `npm install`' \
  || fail "human hotspots must show the action hint: $HOT_HUMAN"
echo "$HOT_HUMAN" | grep -qF 'reclaim estimate:' \
  || fail "human hotspots must show the reclaim estimate: $HOT_HUMAN"

# Error paths: unknown scan is a clean not-found on every surface.
mcp_error_text get_hotspots '{"scanId":"e7ae86e2-308b-444c-8a3d-cd21467ab442"}' \
  | grep -qi 'not found' || fail "MCP get_hotspots must surface not-found"
rc=0; "$BIN/phantom" hotspots --scan e7ae86e2-308b-444c-8a3d-cd21467ab442 >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || fail "CLI hotspots of unknown scan: want exit 3, got $rc"
echo "e2e: hotspots parity, full-walk classification, and categories hold"

# ---------------------------------------------------------------------------
# 11b. Scan diff (phantom-081): grow HOT by a file, rescan, diff old->new.
# The three surfaces must agree byte-for-byte, and the deltas must be exact.
# ---------------------------------------------------------------------------
perl -e 'print "\x05" x (3*1024*1024)' > "$HOT/added-3mb.bin"   # +3 MiB, new row
HOT2_ID="$("$BIN/phantom" scan "$HOT" --json | jq -r .id)"

DIFF_HTTP="$(http_get "$BASE/scans/$HOT_ID/diff/$HOT2_ID" | jq -Sc .)"
DIFF_CLI="$("$BIN/phantom" diff "$HOT_ID" "$HOT2_ID" --json | jq -Sc .)"
DIFF_MCP="$(mcp_result diff_scans "{\"scanA\":\"$HOT_ID\",\"scanB\":\"$HOT2_ID\"}" | jq -Sc .)"
[ "$DIFF_CLI" = "$DIFF_HTTP" ] || fail "diff: CLI and HTTP disagree:
  cli:  $DIFF_CLI
  http: $DIFF_HTTP"
[ "$DIFF_HTTP" = "$DIFF_MCP" ] || fail "diff: HTTP and MCP disagree:
  http: $DIFF_HTTP
  mcp:  $DIFF_MCP"

# +3 MiB exactly; root grew; the diff echoes both ids and the shared root.
echo "$DIFF_HTTP" | jq -e --argjson mib "$MIB" --arg a "$HOT_ID" --arg b "$HOT2_ID" '
  .scanA == $a and .scanB == $b and .rootPath == "'"$HOT"'"
  and .diskDelta == 3 * $mib
  and .fileCountDelta == 1
  and ([.grown[].path] | index("'"$HOT"'") != null)
  and (.freed | length == 0)
' >/dev/null || fail "diff totals wrong: $DIFF_HTTP"

# Direction is positional: swapping the ids negates the disk delta.
http_get "$BASE/scans/$HOT2_ID/diff/$HOT_ID" \
  | jq -e --argjson mib "$MIB" '.diskDelta == -(3 * $mib)' >/dev/null \
  || fail "diff direction not positional"

# Different roots is a 400 (meaningless comparison), not a 409. SCAN_ID is
# the fixture-tree scan from section 1 — a different root than HOT.
MISMATCH_CODE="$(curl -s -o /dev/null -w '%{http_code}' \
  -H "x-api-key: $API_KEY" "$BASE/scans/$HOT_ID/diff/$SCAN_ID")"
[ "$MISMATCH_CODE" = 400 ] || fail "diff of different roots: want 400, got $MISMATCH_CODE"
echo "e2e: scan diff parity holds across HTTP, CLI, MCP"

# ---------------------------------------------------------------------------
# 12. Query-value encoding (freeze review R3): a directory whose name carries
# URL-hostile punctuation must flow through CLI, MCP, and raw HTTP alike.
# Before the client-side percent-encoding fix, the `&` truncated the query
# and the `%` corrupted it — the CLI/MCP calls below 404'd or mis-filtered.
# ---------------------------------------------------------------------------
PUNCT="$WORK/punct"
ODD="$PUNCT/odd & name +#42%"
mkdir -p "$ODD"
perl -e 'print "\x03" x (1024*1024)' > "$ODD/weird.bin"

PUNCT_SCAN="$("$BIN/phantom" scan "$PUNCT" --json)"
PUNCT_ID="$(echo "$PUNCT_SCAN" | jq -r .id)"
[ "$(echo "$PUNCT_SCAN" | jq -r .status)" = "complete" ] || fail "punct scan did not complete"

# CLI tree at the odd path (the CLI must encode the ?path= value).
"$BIN/phantom" tree --scan "$PUNCT_ID" --path "$ODD" --json \
  | jq -e '[.[].name] == ["weird.bin"]' >/dev/null \
  || fail "CLI tree through the punctuated path failed"

# Raw-HTTP parity for the same view (curl encodes via --data-urlencode).
TREE_CLI="$("$BIN/phantom" tree --scan "$PUNCT_ID" --path "$ODD" --json | jq -Sc .)"
TREE_HTTP="$(curl -sf -G -H "x-api-key: $API_KEY" \
  --data-urlencode "path=$ODD" "$BASE/scans/$PUNCT_ID/tree" | jq -Sc .)"
[ "$TREE_CLI" = "$TREE_HTTP" ] || fail "punctuated tree: CLI and HTTP disagree:
  cli:  $TREE_CLI
  http: $TREE_HTTP"

# MCP treemap re-rooted at the odd directory (the MCP must encode root=).
mcp_result get_treemap "{\"scanId\":\"$PUNCT_ID\",\"root\":$(printf '%s' "$ODD" | jq -Rs .)}" \
  | jq -e --arg odd "$ODD" '.rootPath == $odd' >/dev/null \
  || fail "MCP treemap did not re-root at the punctuated path"

# Entry lookup for the odd FILE over raw HTTP, and the same row via the
# CLI's files listing — the persisted path round-trips exactly.
curl -sf -G -H "x-api-key: $API_KEY" --data-urlencode "path=$ODD/weird.bin" \
  "$BASE/scans/$PUNCT_ID/entry" | jq -e --arg p "$ODD/weird.bin" '.path == $p' >/dev/null \
  || fail "entry lookup lost the punctuated path"
"$BIN/phantom" top --scan "$PUNCT_ID" --json \
  | jq -e --arg p "$ODD/weird.bin" '[.[].path] == [$p]' >/dev/null \
  || fail "files listing lost the punctuated path"
echo "e2e: punctuated paths survive every client's query encoding"

echo "e2e: PASS"
