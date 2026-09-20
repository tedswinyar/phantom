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
# 0. Discovery (2026-09-09): the API PUBLISHES its bound URL beside the test
# database, byte-identical to the announcement, and a client with no
# PHANTOM_API_URL finds the server through the prod-profile file under
# $HOME — the day another vendor's agent sat on 8768 the default would have
# been it. HOME is redirected to a scratch dir so the real ~ is never read.
# ---------------------------------------------------------------------------
[ "$(cat "$WORK/api_url")" = "$BASE" ] || fail "api_url must hold the announced URL: $(cat "$WORK/api_url" 2>&1)"
FAKEHOME="$WORK/fakehome"
mkdir -p "$FAKEHOME/Library/Application Support/phantom"
cp "$WORK/api_url" "$FAKEHOME/Library/Application Support/phantom/api_url"
cp "$WORK/api_key" "$FAKEHOME/Library/Application Support/phantom/api_key"
env -u PHANTOM_API_URL HOME="$FAKEHOME" PHANTOM_KEY_FILE="$WORK/api_key" "$BIN/phantom" health | jq -e '.status == "ok"' >/dev/null \
  || fail "CLI must discover the API through the published api_url file when PHANTOM_API_URL is unset"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health","arguments":{}}}' \
  | env -u PHANTOM_API_URL HOME="$FAKEHOME" PHANTOM_KEY_FILE="$WORK/api_key" "$BIN/phantom-mcp" 2>/dev/null \
  | jq -e '.result.structuredContent.status == "ok"' >/dev/null \
  || fail "MCP must discover the API through the published api_url file when PHANTOM_API_URL is unset"
# A clobbered file is not believed: the client falls back to the default and
# says where the URL came from when it cannot connect.
printf 'https://example.com/\n' > "$FAKEHOME/Library/Application Support/phantom/api_url"
set +e
DISC_ERR="$(env -u PHANTOM_API_URL HOME="$FAKEHOME" PHANTOM_KEY_FILE="$WORK/api_key" "$BIN/phantom" --api-url http://127.0.0.1:1 health 2>&1 >/dev/null)"; DISC_RC=$?
set -e
[ "$DISC_RC" = "4" ] || fail "an unreachable --api-url must exit 4 (got $DISC_RC)"
echo "$DISC_ERR" | grep -q "PHANTOM_API_URL" || fail "the cannot-reach diagnostic must name the URL's source: $DISC_ERR"
echo "e2e: discovery — the published api_url file steers the CLI and MCP; junk is not believed"

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
EXPECTED_TOOLS="$(printf '%s\n' cancel_scan diff_scans explain_path find_large_files find_stale_projects \
  get_growth get_hotspots get_space_by_type get_treemap get_volume_status health list_scans plan_reclaim \
  scan_directory scan_status verify_reclaim)"
MCP_TOOLS="$(printf '%s\n' '{"jsonrpc":"2.0","id":9,"method":"tools/list"}' \
  | "$BIN/phantom-mcp" | jq -r '.result.tools[].name' | sort)"
[ "$MCP_TOOLS" = "$EXPECTED_TOOLS" ] || fail "MCP tool set drifted from the parity table:
  want: $(echo "$EXPECTED_TOOLS" | tr '\n' ' ')
  got:  $(echo "$MCP_TOOLS" | tr '\n' ' ')"

# Every operation must also be a reachable CLI subcommand (--help exits 0).
for cli in "scan" "scans list" "scans show" "scans cancel" "scans delete" \
  "top" "tree" "types" "hotspots" "diff" "plan" "verify" "explain" "stale" "volume" "growth" "health"; do
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
# An APFS pure clone of big.bin (v1.1, phantom-mkn.1): st_blocks says a full
# MiB for it too, yet the clone group is charged ONCE and deleting the clone
# frees nothing. big.bin sorts first in the walk, so it keeps the row.
cp -c "$TREE/big.bin" "$TREE/sub/big-clone.bin"
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
echo "$SCAN_JSON" | jq -e '.fileCount == 6 and .dirCount == 3 and .errorCount == 0' >/dev/null \
  || fail "scan totals wrong (want 6 files, 3 dirs): $SCAN_JSON"
# v1.1 totals: what deleting the tree frees. Every sharing group is wholly
# inside the tree, and the decmpfs-compressed cloud.dat owns its blocks
# (compressed files report PRIVATESIZE 0 to the kernel — the walker must not
# read that as shared), so the root is fully private.
echo "$SCAN_JSON" | jq -e '.totalPrivateSize == .totalDiskSize and .totalSharedSize == 0' >/dev/null \
  || fail "share totals wrong (want private == disk, shared 0): $SCAN_JSON"
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

# Three sizes (v1.1): big.bin is a pure clone (its twin is big-clone.bin),
# so it carries a cloneId, privateSize 0, sharedSize == diskSize and both
# clone flags; the clone has no row of its own (first member charged), and
# medium.log — an ordinary file — is fully private with no flags. The
# identical privateSize must reach every surface: the files listing parity
# above already compared CLI/HTTP/MCP byte-for-byte, so pin the VALUES here.
echo "$BIG_ENTRY" | jq -e '
  .cloneId != null and .privateSize == 0 and .sharedSize == .diskSize
  and .flags == ["mayShareBlocks", "sharesAllBlocks"]
' >/dev/null || fail "big.bin must read as a pure clone: $BIG_ENTRY"
# The clone group counts ONCE: the scan total is the persisted rows' disk
# sizes (big + cloud + medium, the clone contributing 0) plus the two
# filtered sub-MiB files — under a megabyte of remainder. A charged clone
# would add exactly 1 MiB more. (cloud.dat's decmpfs allocation varies run
# to run, so the check is relative, not a constant.)
echo "$FILES_HTTP" | jq -e --argjson mib "$MIB" --argjson total "$(echo "$SCAN_JSON" | jq .totalDiskSize)" '
  ($total - ([.[].diskSize] | add)) < $mib
' >/dev/null || fail "clone must not be charged twice: total $(echo "$SCAN_JSON" | jq .totalDiskSize) vs rows $(echo "$FILES_HTTP" | jq -c '[.[].diskSize]')"
# cloud.dat is COMPRESSED, not cloned and not a cloud placeholder: it owns
# its blocks and the row says why.
echo "$FILES_HTTP" | jq -e '
  [.[] | select(.name == "cloud.dat")][0]
  | .privateSize == .diskSize and .sharedSize == 0 and .cloneId == null and (.flags | index("compressed") != null)
' >/dev/null || fail "cloud.dat must read as compressed and fully private: $(echo "$FILES_HTTP" | jq -c '.[] | select(.name == "cloud.dat")')"
echo "$FILES_HTTP" | jq -e '[.[].name] | index("big-clone.bin") == null' >/dev/null \
  || fail "the clone's row must not persist (the group is charged once)"
echo "$FILES_MCP" | jq -e '
  ([.[] | select(.name == "big.bin")][0] | .privateSize == 0 and .cloneId != null)
  and ([.[] | select(.name == "medium.log")][0] | .privateSize == .diskSize and .sharedSize == 0 and .cloneId == null and .flags == [])
' >/dev/null || fail "MCP file rows must carry the same privateSize/cloneId/flags: $FILES_MCP"
echo "$FILES_CLI" | jq -e '[.[] | select(.name == "big.bin")][0].privateSize == 0' >/dev/null \
  || fail "CLI file rows must carry privateSize"

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
TYPES_HUMAN="$("$BIN/phantom" types --scan "$SCAN_ID")"
echo "$TYPES_HUMAN" | head -1 | grep -qF '.dat' \
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
TREE2_HUMAN="$("$BIN/phantom" tree --scan "$SCAN_ID" --depth 2)"
echo "$TREE2_HUMAN" | grep -qF 'medium.log' \
  || fail "human tree at depth 2 must render sub/medium.log"

# Directory aggregation over the wire: sub carries medium.log AND filtered
# tiny.rs bytes; the scan root's aggregate equals the scan's own total.
echo "$TREE_HTTP" | jq -e --argjson mib "$MIB" '
  [.[] | select(.name == "sub")][0].diskSize > 2 * $mib
' >/dev/null || fail "sub/ aggregate must include its filtered small file"
ROOT_ENTRY="$(http_get "$BASE/scans/$SCAN_ID/entry?path=$TREE")"
[ "$(echo "$ROOT_ENTRY" | jq .diskSize)" = "$(echo "$SCAN_JSON" | jq .totalDiskSize)" ] \
  || fail "root aggregate != scan total"
# The directory split (phantom-mkn.2): sub/ holds ONE member of the clone
# group, so the group's megabyte is SHARED there (deleting sub/ frees only
# medium.log + tiny.rs); the root holds both members, so it is fully
# private. The du-model diskSize charges the group to big.bin's dir (root).
SUB_ENTRY="$(http_get "$BASE/scans/$SCAN_ID/entry?path=$TREE/sub")"
echo "$SUB_ENTRY" | jq -e --argjson mib "$MIB" '
  .sharedSize == $mib and .privateSize == (.diskSize) and .privateSize > 2 * $mib
' >/dev/null || fail "sub/ split wrong (want shared == 1 MiB, private == its du size): $SUB_ENTRY"
echo "$ROOT_ENTRY" | jq -e '.sharedSize == 0 and .privateSize == .diskSize' >/dev/null \
  || fail "root split wrong (want fully private): $ROOT_ENTRY"
# Human tree shows the shared annotation only where it is material.
TREE_HUMAN="$("$BIN/phantom" tree --scan "$SCAN_ID" --depth 1)"
echo "$TREE_HUMAN" | grep -F 'sub/' | grep -qF 'shared]' \
  || fail "human tree must annotate sub/ as partly shared"
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
echo "$SCAN2_JSON" | jq -e '.fileCount == 6' >/dev/null \
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

# MCP scan_status is the poll surface for a no-wait scan (mkn.8): it answers
# the scan view by id, and requires the id (no latest-completed default).
mcp_result scan_status "{\"scanId\":\"$SCAN3_ID\"}" | jq -e --arg id "$SCAN3_ID" '.id == $id' >/dev/null \
  || fail "MCP scan_status must answer the named scan"
mcp_error_text scan_status '{}' | grep -q 'scanId' \
  || fail "MCP scan_status without scanId must say so"
mcp_error_text scan_status '{"scanId":"e7ae86e2-308b-444c-8a3d-cd21467ab442"}' | grep -qi 'not found' \
  || fail "MCP scan_status must surface not-found"
# cancel_scan on a terminal scan is the API's 409, as a tool error.
mcp_error_text cancel_scan "{\"scanId\":\"$SCAN2_ID\"}" | grep -q 'cannot cancel' \
  || fail "MCP cancel_scan of a terminal scan must carry the reason"
mcp_error_text cancel_scan '{}' | grep -q 'scanId' \
  || fail "MCP cancel_scan without scanId must say so"

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
EMPTY_HUMAN="$("$BIN/phantom" hotspots --scan "$SCAN_ID")"
echo "$EMPTY_HUMAN" | grep -qF 'no hotspots found' \
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
# v1.1 honesty fields (phantom-mkn.4): no lockfile beside this node_modules,
# so the tier is CAUTION and the why says so; the rebuild is a download of
# about the group's size; no probe ran, so toolEstimate is present-as-null.
echo "$HOT_HTTP" | jq -e '
  .groups[0].riskTier == "caution"
  and (.groups[0].why | test("no lockfile \\(package-lock.json"))
  and (.groups[0].why | endswith("."))
  and .groups[0].rebuildCost.kind == "download"
  and (.groups[0].rebuildCost.estimate | startswith("re-download"))
  and (.groups[0] | has("toolEstimate")) and .groups[0].toolEstimate == null
' >/dev/null || fail "tier/why/rebuildCost wrong: $HOT_HTTP"
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
echo "$HOT_HUMAN" | grep -qE 'CAUTION +\[regenerableArtifact\]' \
  || fail "human hotspots must lead with the tier badge: $HOT_HUMAN"
echo "$HOT_HUMAN" | grep -qF 'why: installed JavaScript dependencies' \
  || fail "human hotspots must show the why sentence: $HOT_HUMAN"
echo "$HOT_HUMAN" | grep -qF 'rebuild: re-download' \
  || fail "human hotspots must show the rebuild cost: $HOT_HUMAN"
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
# 11a. The cloned tree reclaims ~nothing (phantom-mkn.1, the plan's mutation
# #2 at e2e level): a Finder-duplicated project whose node_modules is a
# hotspot but whose every file is a pure clone of the original's sources.
# du says a megabyte; deleting frees only the copy's own tiny file.
# ---------------------------------------------------------------------------
DUP="$WORK/dup"
mkdir -p "$DUP/orig/src" "$DUP/copy/node_modules"
perl -e 'print "\x06" x (1024*1024)' > "$DUP/orig/src/data.bin"
cp -c "$DUP/orig/src/data.bin" "$DUP/copy/node_modules/data.bin"
printf 'own bytes' > "$DUP/copy/node_modules/own.js"

DUP_SCAN="$("$BIN/phantom" scan "$DUP" --json)"
DUP_ID="$(echo "$DUP_SCAN" | jq -r .id)"
[ "$(echo "$DUP_SCAN" | jq -r .status)" = "complete" ] || fail "dup scan did not complete"
echo "$DUP_SCAN" | jq -e --argjson mib "$MIB" '.totalDiskSize < 2 * $mib' >/dev/null \
  || fail "the clone pair must be charged once: $DUP_SCAN"

DUP_HTTP="$(http_get "$BASE/scans/$DUP_ID/hotspots" | jq -Sc .)"
DUP_CLI="$("$BIN/phantom" hotspots --scan "$DUP_ID" --json | jq -Sc .)"
DUP_MCP="$(mcp_result get_hotspots "{\"scanId\":\"$DUP_ID\"}" | jq -Sc .)"
[ "$DUP_CLI" = "$DUP_HTTP" ] || fail "cloned hotspots: CLI and HTTP disagree:
  cli:  $DUP_CLI
  http: $DUP_HTTP"
[ "$DUP_HTTP" = "$DUP_MCP" ] || fail "cloned hotspots: HTTP and MCP disagree:
  http: $DUP_HTTP
  mcp:  $DUP_MCP"
echo "$DUP_HTTP" | jq -e --argjson mib "$MIB" '
  (.groups | length == 1) and .groups[0].ruleId == "node-modules"
  and .groups[0].diskSize >= $mib
  and .groups[0].privateSize < 8192 and .groups[0].privateSize > 0
  and .reclaimEstimate == .groups[0].privateSize
' >/dev/null || fail "cloned node_modules must reclaim ~0 (du says 1 MiB): $DUP_HTTP"
# Capture, THEN grep: under pipefail a `phantom … | grep -q` race (grep
# exits on its first match, the CLI gets EPIPE) fails the pipeline even when
# the text was there (bit verify on 2026-09-07 once the human view grew).
DUP_HUMAN="$("$BIN/phantom" hotspots --scan "$DUP_ID")"
echo "$DUP_HUMAN" | grep -qF 'deleting frees' \
  || fail "human hotspots must show the deletion-honest number when it differs"
echo "e2e: a cloned tree reclaims ~nothing on every surface"

# ---------------------------------------------------------------------------
# 11d. Gate G2 (v1.1 Phase 2, phantom-mkn.21): the classifier over the
# checked-in fixture projects — one per project type plus decoys with the
# same artifact names and NO detection file — produces EXACTLY the expected
# set of (ruleId, root) pairs and nothing from decoys/. Mutation: delete a
# detection file under tests/fixtures/projects and the set shrinks.
# ---------------------------------------------------------------------------
PROJ="$ROOT_DIR/tests/fixtures/projects"
PROJ_SCAN="$("$BIN/phantom" scan "$PROJ" --json)"
PROJ_ID="$(echo "$PROJ_SCAN" | jq -r .id)"
[ "$(echo "$PROJ_SCAN" | jq -r .status)" = "complete" ] || fail "fixture-projects scan did not complete"
PROJ_HTTP="$(http_get "$BASE/scans/$PROJ_ID/hotspots" | jq -Sc .)"
PROJ_CLI="$("$BIN/phantom" hotspots --scan "$PROJ_ID" --json | jq -Sc .)"
PROJ_MCP="$(mcp_result get_hotspots "{\"scanId\":\"$PROJ_ID\"}" | jq -Sc .)"
[ "$PROJ_CLI" = "$PROJ_HTTP" ] && [ "$PROJ_HTTP" = "$PROJ_MCP" ] \
  || fail "fixture-projects hotspots: surfaces disagree"
GOT_SET="$(echo "$PROJ_HTTP" | jq -r --arg p "$PROJ" \
  '[.groups[] | .ruleId as $r | .topPaths[] | "\($r) \(ltrimstr($p))"] | sort | .[]')"
WANT_SET="$(printf '%s\n' \
  'bazel-output /bazel/bazel-out' 'cabal-dist /cabal/dist-newstyle' 'cargo-target /cargo/target' \
  'cmake-build /cmake/cmake-build-debug' 'cocoapods-pods /cocoapods/Pods' 'composer-vendor /composer/vendor' \
  'dotnet-bin-obj /dotnet/obj' 'elixir-build /elixir/_build' 'flutter-build /flutter/.dart_tool' \
  'go-vendor /go/vendor' 'godot-import /godot/.godot' 'gradle-build /gradle/build' \
  'js-build /js-build/build' 'js-dist /js-dist/dist' 'jupyter-checkpoints /jupyter/.ipynb_checkpoints' \
  'maven-target /maven/target' 'next-build /next/.next' 'node-modules /node/node_modules' \
  'pixi-env /pixi/.pixi' 'python-bytecode /python-bytecode/__pycache__' 'python-caches /python-caches/.pytest_cache' \
  'python-venv /python-venv/.venv' 'react-native-cache /react-native/.expo' 'sbt-target /sbt/target' \
  'stack-work /stack/.stack-work' 'swiftpm-build /swiftpm/.build' 'terraform-providers /terraform/.terraform' \
  'turborepo-cache /turborepo/.turbo' 'unity-library /unity/Library' 'unreal-intermediate /unreal/Intermediate' \
  'zig-cache /zig/zig-out' | sort)"
[ "$GOT_SET" = "$WANT_SET" ] || fail "G2: classifier set over the fixture projects drifted:
  want: $(echo "$WANT_SET" | tr '\n' ',')
  got:  $(echo "$GOT_SET" | tr '\n' ',')"
echo "$PROJ_HTTP" | jq -e '[.groups[].topPaths[] | select(test("/decoys/"))] | length == 0' >/dev/null \
  || fail "G2: a decoy classified: $PROJ_HTTP"
echo "$PROJ_HTTP" | jq -e '([.groups[] | select(.category != "regenerableArtifact")] | length == 0) and .reviewDiskSize == 0' >/dev/null \
  || fail "G2: fixture projects must classify regenerable only: $PROJ_HTTP"
# Lockfile gate on the wire: the cargo fixture HAS Cargo.lock (safe); the
# gradle fixture has no lockfile concept (safe); the go fixture has go.sum
# (safe). Every fixture with a lockfile carries one, so no group is caution.
echo "$PROJ_HTTP" | jq -e '
  ([.groups[] | select(.ruleId == "cargo-target")][0].riskTier == "safe")
  and ([.groups[] | select(.ruleId == "gradle-build")][0].riskTier == "safe")
  and ([.groups[] | select(.riskTier != "safe")] | length == 0)
' >/dev/null || fail "G2: tiers over the fixture projects: $PROJ_HTTP"
http_get "$BASE/scans/$PROJ_ID/entry?path=$PROJ/decoys/target" | jq -e '.category == null' >/dev/null \
  || fail "a bare target/ must stay uncategorized"
"$BIN/phantom" scans delete "$PROJ_ID" >/dev/null
echo "e2e: G2 holds — exact classifier set over the fixture projects, zero decoys"

# ---------------------------------------------------------------------------
# 11e. Phase 2 request knobs on every write surface: --older / olderThan is
# validated at request time (400 → CLI exit 1), and the opt-in flags are
# accepted by CLI and MCP (off by default: the tool estimate above was null).
# ---------------------------------------------------------------------------
rc=0; "$BIN/phantom" scan "$HOT" --older 3m >/dev/null 2>"$WORK/older.err" || rc=$?
[ "$rc" = 1 ] || fail "bad --older: want exit 1, got $rc"
grep -q 'olderThan' "$WORK/older.err" || fail "bad --older must carry the real reason: $(cat "$WORK/older.err")"
OLD_ID="$("$BIN/phantom" scan "$HOT" --older 3M --json | jq -r .id)"
"$BIN/phantom" hotspots --scan "$OLD_ID" --json | jq -e '.groups[0].ruleId == "node-modules"' >/dev/null \
  || fail "--older 3M scan must classify"
"$BIN/phantom" scans delete "$OLD_ID" >/dev/null
SCAN_HELP_E="$("$BIN/phantom" scan --help)"
echo "$SCAN_HELP_E" | grep -q -- '--verify-locks' || fail "CLI scan must offer --verify-locks"
echo "$SCAN_HELP_E" | grep -q -- '--tool-estimates' || fail "CLI scan must offer --tool-estimates"
mcp_error_text scan_directory "{\"path\":\"$HOT\",\"olderThan\":\"never\"}" | grep -q 'olderThan' \
  || fail "MCP scan_directory must surface the olderThan validation error"
MCP_OLD="$(mcp_result scan_directory "{\"path\":\"$HOT\",\"olderThan\":\"12w\",\"toolEstimates\":true}")"
[ "$(echo "$MCP_OLD" | jq -r .status)" = "complete" ] || fail "MCP scan with olderThan/toolEstimates did not complete"
# toolEstimates on a tree with no probe-backed rows changes nothing: still null.
mcp_result get_hotspots "{\"scanId\":\"$(echo "$MCP_OLD" | jq -r .id)\"}" \
  | jq -e '.groups[0].toolEstimate == null' >/dev/null || fail "no probe row, yet a toolEstimate appeared"
"$BIN/phantom" scans delete "$(echo "$MCP_OLD" | jq -r .id)" >/dev/null
echo "e2e: Phase 2 request knobs validate and flow through CLI and MCP"

# ---------------------------------------------------------------------------
# 11c. One filesystem by default (phantom-jsz): /System/Volumes holds only
# mount points (Data, Preboot, VM, …). Scanning it walks NOTHING beneath
# them — zero files — and the boundary is visible on the rows. Mutation
# target: drop the mount-point check and this walks the whole data volume.
# ---------------------------------------------------------------------------
VOL_SCAN="$("$BIN/phantom" scan /System/Volumes --json)"
VOL_ID="$(echo "$VOL_SCAN" | jq -r .id)"
[ "$(echo "$VOL_SCAN" | jq -r .status)" = "complete" ] || fail "/System/Volumes scan did not complete"
echo "$VOL_SCAN" | jq -e '.fileCount == 0 and .totalDiskSize == 0' >/dev/null \
  || fail "mount points must not be descended by default: $VOL_SCAN"
"$BIN/phantom" tree --scan "$VOL_ID" --depth 1 --json \
  | jq -e '[.[] | select(.name == "Data")][0] | .isDir and (.flags | index("mountPoint") != null)' >/dev/null \
  || fail "the Data mount point must be a flagged row"
SCAN_HELP="$("$BIN/phantom" scan --help)"
echo "$SCAN_HELP" | grep -q -- '--cross-volumes' || fail "CLI scan must offer --cross-volumes"
mcp_result scan_directory "{\"path\":\"/System/Volumes\",\"crossVolumes\":false}" \
  | jq -e '.fileCount == 0' >/dev/null || fail "MCP scan_directory must accept crossVolumes"
"$BIN/phantom" scans delete "$VOL_ID" >/dev/null
echo "e2e: one-filesystem default holds; mount points are visible boundaries"

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

# --since (v1.1 Phase 4, phantom-mkn.23.1): the newest completed scan is
# HOT2; a scan-id baseline reproduces the pair diff byte-for-byte; a
# duration no scan satisfies (HOT and HOT2 are seconds apart) is exit 3
# naming the oldest; junk is a usage error (2); positionals conflict (2).
SINCE_CLI="$("$BIN/phantom" diff --since "$HOT_ID" --json | jq -Sc .)"
[ "$SINCE_CLI" = "$DIFF_HTTP" ] || fail "diff --since <id> must equal the pair diff:
  since: $SINCE_CLI
  http:  $DIFF_HTTP"
set +e
SINCE_ERR="$("$BIN/phantom" diff --since 1d 2>&1 >/dev/null)"; SINCE_RC=$?
set -e
[ "$SINCE_RC" = "3" ] || fail "diff --since 1d with no scan that old must exit 3 (got $SINCE_RC: $SINCE_ERR)"
echo "$SINCE_ERR" | grep -q "$HOT_ID" || fail "diff --since must name the oldest available scan: $SINCE_ERR"
set +e
"$BIN/phantom" diff --since soon >/dev/null 2>&1; SINCE_RC=$?
set -e
[ "$SINCE_RC" = "2" ] || fail "diff --since soon must be a usage error (got $SINCE_RC)"
set +e
"$BIN/phantom" diff "$HOT_ID" "$HOT2_ID" --since 1d >/dev/null 2>&1; SINCE_RC=$?
set -e
[ "$SINCE_RC" = "2" ] || fail "diff positionals + --since must conflict (got $SINCE_RC)"
echo "e2e: scan diff parity holds across HTTP, CLI, MCP; --since resolves a baseline"

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

# ---------------------------------------------------------------------------
# 13. MCP protocol currency (v1.1 Phase 3, phantom-mkn.6): version
# negotiation, annotations + outputSchema on every tool, structuredContent
# beside the text, the concise projection, and the argument validation.
# The TEXT content is still the verbatim body — every parity check above
# already proved that — so these pin only the additive envelope.
# ---------------------------------------------------------------------------
mcp_raw() { printf '%s\n' "$1" | "$BIN/phantom-mcp"; }

# initialize echoes a supported revision and falls back to the latest.
mcp_raw '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
  | jq -e '.result.protocolVersion == "2024-11-05"' >/dev/null \
  || fail "initialize must echo a supported protocolVersion (2024-11-05)"
mcp_raw '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}' \
  | jq -e '.result.protocolVersion == "2025-06-18"' >/dev/null \
  || fail "initialize must fall back to the latest supported protocolVersion"

# Every tool: title, the four annotation hints, an object outputSchema.
TOOLS_JSON="$(mcp_raw '{"jsonrpc":"2.0","id":9,"method":"tools/list"}' | jq -c '.result.tools')"
echo "$TOOLS_JSON" | jq -e '
  all(.[]; (.title | type) == "string"
    and (.annotations.readOnlyHint | type) == "boolean"
    and (.annotations.destructiveHint == false)
    and (.annotations.idempotentHint | type) == "boolean"
    and (.annotations.openWorldHint == false)
    and (.outputSchema.type == "object"))' >/dev/null \
  || fail "every MCP tool must carry title, annotations and an object outputSchema: $TOOLS_JSON"
echo "$TOOLS_JSON" | jq -e '
  ([.[] | select(.annotations.readOnlyHint == false) | .name] | sort == ["cancel_scan","plan_reclaim","scan_directory","verify_reclaim"])
  and ([.[] | select(.annotations.idempotentHint == false) | .name] | sort == ["plan_reclaim","scan_directory","verify_reclaim"])
  and all(.[]; .annotations.destructiveHint == false)' >/dev/null \
  || fail "writers are scan_directory/cancel_scan/plan_reclaim/verify_reclaim; non-idempotent are the three that make something new; nothing is destructive"
echo "$TOOLS_JSON" | jq -e '
  [.[] | select(._meta["anthropic/maxResultSizeChars"] != null) | .name] | sort == ["find_large_files","get_treemap"]' >/dev/null \
  || fail "the result-size hint belongs on exactly get_treemap and find_large_files"
# Deterministic order: scan first, health last.
echo "$TOOLS_JSON" | jq -e '(.[0].name == "get_volume_status") and (.[1].name == "scan_directory") and (.[-1].name == "health")' >/dev/null \
  || fail "tools/list order changed"

# structuredContent is the parsed text for object bodies, and wraps the
# bare-array bodies (list_scans -> {scans}, get_space_by_type -> {types}).
HOT_RESP="$(mcp_call get_hotspots "{\"scanId\":\"$PUNCT_ID\"}")"
echo "$HOT_RESP" | jq -e '.result.structuredContent == (.result.content[0].text | fromjson)' >/dev/null \
  || fail "get_hotspots structuredContent must equal the text content parsed"
TYPES_RESP="$(mcp_call get_space_by_type "{\"scanId\":\"$PUNCT_ID\"}")"
echo "$TYPES_RESP" | jq -e '(.result.content[0].text | fromjson | type) == "array"
  and (.result.structuredContent.types == (.result.content[0].text | fromjson))' >/dev/null \
  || fail "get_space_by_type: text stays a bare array, structuredContent wraps it as {types}"
mcp_call list_scans '{}' | jq -e '(.result.content[0].text | fromjson | type) == "array"
  and (.result.structuredContent.scans == (.result.content[0].text | fromjson))' >/dev/null \
  || fail "list_scans: text stays a bare array, structuredContent wraps it as {scans}"

# Concise keeps exactly the acting fields; detailed is the verbatim body.
CONCISE_FILES="$(mcp_result find_large_files "{\"scanId\":\"$PUNCT_ID\",\"responseFormat\":\"concise\"}")"
echo "$CONCISE_FILES" | jq -e '(.files | length) >= 1
  and all(.files[]; (keys | sort) == ["diskSize","fileType","path","privateSize"])
  and has("nextCursor")' >/dev/null \
  || fail "concise find_large_files must keep exactly path/diskSize/privateSize/fileType: $CONCISE_FILES"
DETAILED_FILES="$(mcp_result find_large_files "{\"scanId\":\"$PUNCT_ID\",\"responseFormat\":\"detailed\"}" | jq -Sc .files)"
DEFAULT_FILES="$(mcp_result find_large_files "{\"scanId\":\"$PUNCT_ID\"}" | jq -Sc .files)"
[ "$DETAILED_FILES" = "$DEFAULT_FILES" ] || fail "responseFormat detailed must equal the default"
mcp_result get_treemap "{\"scanId\":\"$PUNCT_ID\",\"responseFormat\":\"concise\"}" \
  | jq -e 'all(.rects[]; (keys | sort) == ["depth","isDir","path","size"])' >/dev/null \
  || fail "concise get_treemap must drop the geometry"
mcp_error_text find_large_files '{"responseFormat":"brief"}' | grep -q 'concise' \
  || fail "an invalid responseFormat must be refused, naming the valid values"
# A waited scan_directory with _meta.progressToken emits notifications/progress
# lines BEFORE the response, echoing the token; without a token, none.
PROGRESS_OUT="$(printf '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"scan_directory","arguments":{"path":"%s"},"_meta":{"progressToken":"e2e-tok"}}}\n' "$TREE" | "$BIN/phantom-mcp")"
echo "$PROGRESS_OUT" | tail -1 | jq -e '.id == 5 and (.result.content[0].text | fromjson | .status == "complete")' >/dev/null \
  || fail "the response must come last and be the completed scan: $PROGRESS_OUT"
NOTES="$(echo "$PROGRESS_OUT" | sed '$d')"
if [ -n "$NOTES" ]; then
  echo "$NOTES" | jq -e -s 'all(.[]; .method == "notifications/progress" and .params.progressToken == "e2e-tok" and (.params.progress | type) == "number" and (.params.message | type) == "string" and (has("id") | not))' >/dev/null \
    || fail "every line before the response must be a progress notification echoing the token: $NOTES"
fi
NO_TOKEN_OUT="$(printf '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"scan_directory","arguments":{"path":"%s"}}}\n' "$TREE" | "$BIN/phantom-mcp")"
[ "$(echo "$NO_TOKEN_OUT" | wc -l | tr -d ' ')" = 1 ] || fail "no progressToken, no notifications: $NO_TOKEN_OUT"
echo "e2e: MCP protocol currency — negotiation, annotations, structuredContent, concise, progress"

# ---------------------------------------------------------------------------
# 14. The reclaim loop (v1.1 Phase 3, phantom-mkn.7): scan → plan → run the
# plan's script → rescan → verify, across HTTP, CLI and MCP. The mechanical
# twin of gate G3 (a fresh agent doing the same with only the MCP tools and
# SKILL.md is a human-run gate; this pins the arithmetic and the parity).
# ---------------------------------------------------------------------------
PLANROOT="$WORK/planroot"
PROJDIR="$PLANROOT/proj"
mkdir -p "$PROJDIR/src" "$PROJDIR/target/debug"
printf '[package]\nname = "p"\nversion = "0.1.0"\n' > "$PROJDIR/Cargo.toml"
printf '# lock\n' > "$PROJDIR/Cargo.lock"                 # lockfile present → safe
printf 'fn main() {}\n' > "$PROJDIR/src/main.rs"
perl -e 'print "\x07" x (2*1024*1024)' > "$PROJDIR/target/debug/blob.bin"
BEFORE_JSON="$("$BIN/phantom" scan "$PLANROOT" --json)"
BEFORE_ID="$(echo "$BEFORE_JSON" | jq -r .id)"
[ "$(echo "$BEFORE_JSON" | jq -r .status)" = "complete" ] || fail "plan-root scan did not complete"

# Three surfaces build the same plan (ids and timestamps differ per build).
PLAN_CLI="$("$BIN/phantom" plan --scan "$BEFORE_ID" --json)"
PLAN_HTTP="$(curl -sf -X POST -H "x-api-key: $API_KEY" -H 'content-type: application/json' -d '{}' "$BASE/scans/$BEFORE_ID/plan")"
PLAN_MCP="$(mcp_result plan_reclaim "{\"scanId\":\"$BEFORE_ID\"}")"
norm_plan() { jq -Sc 'del(.planId, .createdAt)'; }
[ "$(echo "$PLAN_CLI" | norm_plan)" = "$(echo "$PLAN_HTTP" | norm_plan)" ] || fail "plan: CLI and HTTP disagree:
  cli:  $PLAN_CLI
  http: $PLAN_HTTP"
[ "$(echo "$PLAN_HTTP" | norm_plan)" = "$(echo "$PLAN_MCP" | norm_plan)" ] || fail "plan: HTTP and MCP disagree"
echo "$PLAN_CLI" | jq -e --arg t "$PROJDIR/target" '
  .itemCount == 1 and .maxTier == "safe" and .items[0].ruleId == "cargo-target"
  and .items[0].riskTier == "safe" and .items[0].paths == [$t]
  and .items[0].expectedFreedBytes >= 2097152 and .expectedFreedBytes == .items[0].expectedFreedBytes
  and .items[0].command == "cargo clean" and .skipped.tracked == 0' >/dev/null || fail "plan content: $PLAN_CLI"
PLAN_ID="$(echo "$PLAN_CLI" | jq -r .planId)"
EXPECTED="$(echo "$PLAN_CLI" | jq -r .expectedFreedBytes)"
# The plan reads back; review is refused; a caution plan is a superset.
curl -sf -H "x-api-key: $API_KEY" "$BASE/plans/$PLAN_ID" | jq -e --arg id "$PLAN_ID" '.planId == $id' >/dev/null \
  || fail "GET /plans/{id} must return the plan"
rc=0; "$BIN/phantom" plan --scan "$BEFORE_ID" --max-tier review >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] || fail "plan --max-tier review must be a usage error (got $rc)"
mcp_error_text plan_reclaim "{\"scanId\":\"$BEFORE_ID\",\"maxTier\":\"review\"}" | grep -q 'never plan items' \
  || fail "MCP plan_reclaim maxTier review must be refused with the reason"
# includeScript carries the script; the CLI --script is the same bytes.
SCRIPT_MCP="$(mcp_result plan_reclaim "{\"scanId\":\"$BEFORE_ID\",\"includeScript\":true}" | jq -r .script)"
echo "$SCRIPT_MCP" | grep -q "^apply '$PROJDIR/target'$" || fail "MCP script must apply the quoted path: $SCRIPT_MCP"
"$BIN/phantom" plan --scan "$BEFORE_ID" --script > "$WORK/plan.sh"
grep -q "^apply '$PROJDIR/target'$" "$WORK/plan.sh" || fail "CLI --script must apply the quoted path"
# Every `phantom plan` mints a NEW plan (by design: a plan is a dated
# promise); the script names its own id, and that is the Trash folder's name.
SCRIPT_PLAN_ID="$(sed -n 's/^# Phantom reclaim plan \(.*\)$/\1/p' "$WORK/plan.sh")"
[ "${#SCRIPT_PLAN_ID}" = 36 ] || fail "script must name its plan id in the header: $(head -3 "$WORK/plan.sh")"
grep -q "^#!/bin/sh$" "$WORK/plan.sh" || fail "script must start with a shebang"
! grep -q 'rm ' "$WORK/plan.sh" || fail "the script must never delete"

# Dry run touches nothing; PHANTOM_APPLY=1 moves target/ into a throwaway HOME's Trash.
mkdir -p "$WORK/home"
DRY="$(HOME="$WORK/home" /bin/sh "$WORK/plan.sh")"
echo "$DRY" | grep -q "would move: $PROJDIR/target" || fail "dry run must announce the move: $DRY"
[ -d "$PROJDIR/target" ] || fail "dry run must not move anything"
[ ! -d "$WORK/home/.Trash" ] || fail "dry run must not create the trash folder"
APPLIED="$(HOME="$WORK/home" PHANTOM_APPLY=1 /bin/sh "$WORK/plan.sh")"
echo "$APPLIED" | grep -q "^moved: $PROJDIR/target" || fail "apply must move: $APPLIED"
[ ! -e "$PROJDIR/target" ] || fail "apply must move target/ away"
[ "$(find "$WORK/home/.Trash/phantom-$SCRIPT_PLAN_ID" -name blob.bin | wc -l | tr -d ' ')" = 1 ] \
  || fail "the bytes must be in the plan's Trash folder, not deleted"
grep -q "$PROJDIR/target" "$WORK/home/.Trash/phantom-$SCRIPT_PLAN_ID.log" || fail "apply must log the move"

# Verify: CLI (explicit --after) and MCP (afterScanId) agree; actual == expected.
AFTER_ID="$("$BIN/phantom" scan "$PLANROOT" --json | jq -r .id)"
VERIFY_CLI="$("$BIN/phantom" verify "$PLAN_ID" --after "$AFTER_ID" --json)"
VERIFY_MCP="$(mcp_result verify_reclaim "{\"planId\":\"$PLAN_ID\",\"afterScanId\":\"$AFTER_ID\"}")"
[ "$(echo "$VERIFY_CLI" | jq -Sc 'del(.verifiedAt)')" = "$(echo "$VERIFY_MCP" | jq -Sc 'del(.verifiedAt)')" ] \
  || fail "verify: CLI and MCP disagree:
  cli: $VERIFY_CLI
  mcp: $VERIFY_MCP"
echo "$VERIFY_CLI" | jq -e --argjson exp "$EXPECTED" --arg b "$BEFORE_ID" --arg a "$AFTER_ID" '
  .beforeScanId == $b and .afterScanId == $a and .expectedFreedBytes == $exp
  and .items[0].actualFreedBytes == $exp and .items[0].afterBytes == 0
  and .actualFreedBytes >= $exp and .withinTolerance == true' >/dev/null \
  || fail "verification must show the whole target/ came back within tolerance: $VERIFY_CLI"
# A rescan that predates the plan is refused; a wrong root is refused.
mcp_error_text verify_reclaim "{\"planId\":\"$PLAN_ID\",\"afterScanId\":\"$BEFORE_ID\"}" | grep -q 'before the plan' \
  || fail "verify with a pre-plan rescan must be refused"
mcp_error_text verify_reclaim "{\"planId\":\"$PLAN_ID\",\"afterScanId\":\"$SCAN_ID\"}" | grep -q "rescan the plan's root" \
  || fail "verify against another root must be refused"
# MCP verify_reclaim with no afterScanId rescans by itself (tiny tree: completes within the wait).
mcp_result verify_reclaim "{\"planId\":\"$PLAN_ID\"}" | jq -e '.withinTolerance == true and (.afterScanId | length) == 36' >/dev/null \
  || fail "verify_reclaim without afterScanId must rescan and verify"
# CLI verify exit code: 0 within tolerance.
"$BIN/phantom" verify "$PLAN_ID" --after "$AFTER_ID" >/dev/null || fail "phantom verify must exit 0 within tolerance"
echo "e2e: the reclaim loop holds — plan parity, dry-run script, Trash move, verified within tolerance"

# ---------------------------------------------------------------------------
# 15. Insight (v1.1 Phase 3, phantom-mkn.9): explain a path, stale projects,
# volume status — three-way parity and the answers' content.
# ---------------------------------------------------------------------------
# A second project whose SOURCES are 200 days old (the artifact is fresh —
# artifact mtimes must not count), scanned into the plan root.
OLDPROJ="$PLANROOT/old"
mkdir -p "$OLDPROJ/src" "$OLDPROJ/target/debug"
printf '[package]\nname = "old"\nversion = "0.1.0"\n' > "$OLDPROJ/Cargo.toml"
printf '# lock\n' > "$OLDPROJ/Cargo.lock"
printf 'fn main() {}\n' > "$OLDPROJ/src/main.rs"
perl -e 'print "\x09" x (3*1024*1024)' > "$OLDPROJ/target/debug/old.bin"
OLDSTAMP="$(perl -MPOSIX -e 'print strftime("%Y%m%d%H%M", localtime(time - 200*86400))')"
touch -t "$OLDSTAMP" "$OLDPROJ/Cargo.toml" "$OLDPROJ/Cargo.lock" "$OLDPROJ/src/main.rs"
INS_ID="$("$BIN/phantom" scan "$PLANROOT" --json | jq -r .id)"

# explain: parity, and the content for a hotspot root and for ordinary content.
EXP_HTTP="$(curl -sf -G -H "x-api-key: $API_KEY" --data-urlencode "path=$OLDPROJ/target" "$BASE/scans/$INS_ID/explain" | jq -Sc .)"
EXP_CLI="$("$BIN/phantom" explain "$OLDPROJ/target" --scan "$INS_ID" --json | jq -Sc .)"
EXP_MCP="$(mcp_result explain_path "{\"scanId\":\"$INS_ID\",\"path\":\"$OLDPROJ/target\"}" | jq -Sc .)"
[ "$EXP_CLI" = "$EXP_HTTP" ] && [ "$EXP_HTTP" = "$EXP_MCP" ] || fail "explain: surfaces disagree:
  cli:  $EXP_CLI
  http: $EXP_HTTP
  mcp:  $EXP_MCP"
echo "$EXP_HTTP" | jq -e '.isDir == true and .category == "staleProjectArtifact"
  and .hotspot.ruleId == "cargo-target" and .hotspot.riskTier == "safe" and .hotspot.matchedBy == "topPath"
  and .privateSize == .diskSize and .dataless == false and .unreadableBelowCount == 0
  and (.summary | test("classified staleProjectArtifact \\(safe\\)"))' >/dev/null \
  || fail "explain content: $EXP_HTTP"
"$BIN/phantom" explain "$OLDPROJ/src" --scan "$INS_ID" --json | jq -e '.hotspot == null and .category == null and (.summary | test("not a hotspot"))' >/dev/null \
  || fail "explain of ordinary content must say so"
mcp_error_text explain_path "{\"scanId\":\"$INS_ID\"}" | grep -q 'path' || fail "explain_path without path must say so"
mcp_error_text explain_path "{\"scanId\":\"$INS_ID\",\"path\":\"/no/such\"}" | grep -qi 'not found' || fail "explain_path unknown path must be not found"

# stale: parity; the 200-day project qualifies at 90d, nothing at 1y.
ST_HTTP="$(http_get "$BASE/scans/$INS_ID/stale" | jq -Sc .)"
ST_CLI="$("$BIN/phantom" stale --scan "$INS_ID" --json | jq -Sc .)"
ST_MCP="$(mcp_result find_stale_projects "{\"scanId\":\"$INS_ID\"}" | jq -Sc .)"
[ "$ST_CLI" = "$ST_HTTP" ] && [ "$ST_HTTP" = "$ST_MCP" ] || fail "stale: surfaces disagree:
  cli:  $ST_CLI
  http: $ST_HTTP
  mcp:  $ST_MCP"
echo "$ST_HTTP" | jq -e --arg r "$OLDPROJ" --arg t "$OLDPROJ/target" '
  .thresholdDays == 90 and .projectsEvaluated == 2 and .unverifiable == 0
  and (.projects | length) == 1 and .projects[0].root == $r
  and (.projects[0].lastActivityDays >= 199 and .projects[0].lastActivityDays <= 201)
  and .projects[0].artifacts[0].path == $t and .projects[0].artifacts[0].category == "staleProjectArtifact"
  and .artifactDiskSize == .projects[0].artifactDiskSize and .artifactDiskSize >= 3145728' >/dev/null \
  || fail "stale content: $ST_HTTP"
"$BIN/phantom" stale --scan "$INS_ID" --older 1y --json | jq -e '.thresholdDays == 365 and .projects == []' >/dev/null \
  || fail "stale at 1y must list nothing"
# At 1d the 200-day project qualifies and the project edited today (0 days) does not.
mcp_result find_stale_projects "{\"scanId\":\"$INS_ID\",\"olderThan\":\"1d\"}" | jq -e --arg r "$OLDPROJ" '(.projects | length) == 1 and .projects[0].root == $r' >/dev/null \
  || fail "stale at 1d must list exactly the old project"
mcp_error_text find_stale_projects "{\"scanId\":\"$INS_ID\",\"olderThan\":\"soon\"}" | grep -q 'olderThan' \
  || fail "find_stale_projects must surface the olderThan grammar error"

# volume: parity on the stable fields; numbers are live, so pin their relations.
# (Phase 4: volumeUsedBytes / purgeable / importantUsage / opportunistic and
# the hidden differences move between calls too.)
VOL_LIVE='del(.usedBytes, .freeBytes, .availableBytes, .volumeUsedBytes, .purgeableBytes, .importantUsageBytes,
  .opportunisticUsageBytes, .hidden.unscannedBytes, .hidden.otherVolumesBytes)'
VOL_HTTP="$(http_get "$BASE/volume" | jq -Sc "$VOL_LIVE")"
VOL_CLI="$("$BIN/phantom" volume --json | jq -Sc "$VOL_LIVE")"
VOL_MCP="$(mcp_result get_volume_status '{}' | jq -Sc "$VOL_LIVE")"
[ "$VOL_CLI" = "$VOL_HTTP" ] && [ "$VOL_HTTP" = "$VOL_MCP" ] || fail "volume: surfaces disagree:
  cli:  $VOL_CLI
  http: $VOL_HTTP
  mcp:  $VOL_MCP"
# Phase 4 (phantom-mkn.12 / 4p3): the volume's own usage (getattrlist) is a
# number, the container arithmetic holds, and without a scan the split's scan
# fields are null. Purgeable space comes from CoreFoundation, which answers for
# a GUI user and NOT for a headless one (the MBP runner's builder returned 0,
# 2026-09-17): with an answer, important >= available and purgeable is the
# difference; without one, both are null — never a confident 0.
http_get "$BASE/volume" | jq -e '.totalBytes > 0 and .usedBytes == .totalBytes - .freeBytes and .availableBytes <= .freeBytes
  and (.volumeUsedBytes | type) == "number" and .volumeUsedBytes <= .usedBytes
  and .hidden.otherVolumesBytes == .usedBytes - .volumeUsedBytes
  and ((.importantUsageBytes == null and .purgeableBytes == null)
       or ((.importantUsageBytes | type) == "number" and .importantUsageBytes >= .availableBytes
           and .purgeableBytes == .importantUsageBytes - .availableBytes))
  and .opportunisticUsageBytes != 0
  and .snapshotCount == null and .snapshots == null
  and .hidden.scanId == null and .hidden.scannedBytes == null and .hidden.unscannedBytes == null
  and .hidden.unreadableCount == null and .hidden.snapshotSuggestion == null and (.hidden.otherUserHomes | type) == "array"
  and (.path == "/System/Volumes/Data" or .path == "/")' >/dev/null || fail "volume content: $(http_get "$BASE/volume")"
http_get "$BASE/volume?path=$WORK" | jq -e --arg w "$WORK" '.path == $w and .totalBytes > 0' >/dev/null \
  || fail "volume must resolve a named path"
mcp_error_text get_volume_status '{"path":"/no/such/mount/anywhere"}' | grep -qi 'not found' \
  || fail "get_volume_status unknown path must be not found"
# The hidden-space split relative to the insight scan: scanned is the scan's
# totalDiskSize, unscanned = this volume's usage − scanned, unreadable = the
# scan's errorCount — on all three surfaces (the path is the scan root, so
# the volume is the scan's whatever the machine's layout).
INS_TOTAL="$(http_get "$BASE/scans/$INS_ID" | jq -r .totalDiskSize)"
HID_HTTP="$(http_get "$BASE/volume?path=$PLANROOT&scanId=$INS_ID" | jq -Sc '.hidden | del(.unscannedBytes, .otherVolumesBytes)')"
HID_CLI="$("$BIN/phantom" volume "$PLANROOT" --scan "$INS_ID" --json | jq -Sc '.hidden | del(.unscannedBytes, .otherVolumesBytes)')"
HID_MCP="$(mcp_result get_volume_status "{\"path\":\"$PLANROOT\",\"scanId\":\"$INS_ID\"}" | jq -Sc '.hidden | del(.unscannedBytes, .otherVolumesBytes)')"
[ "$HID_CLI" = "$HID_HTTP" ] && [ "$HID_HTTP" = "$HID_MCP" ] || fail "hidden space: surfaces disagree:
  cli:  $HID_CLI
  http: $HID_HTTP
  mcp:  $HID_MCP"
http_get "$BASE/volume?path=$PLANROOT&scanId=$INS_ID" | jq -e --arg id "$INS_ID" --arg r "$PLANROOT" --argjson t "$INS_TOTAL" '
  .hidden.scanId == $id and .hidden.scanRootPath == $r and .hidden.scannedBytes == $t
  and .hidden.unscannedBytes == .volumeUsedBytes - $t and .hidden.unreadableCount == 0
  and .hidden.snapshotSuggestion == null' >/dev/null || fail "hidden space content: $(http_get "$BASE/volume?path=$PLANROOT&scanId=$INS_ID")"
# The human view names the split and never hides the suggestion's posture.
VOL_HUMAN="$("$BIN/phantom" volume "$PLANROOT" --scan "$INS_ID")"
echo "$VOL_HUMAN" | grep -q "other volumes in the container" || fail "phantom volume must print the container split: $VOL_HUMAN"
echo "$VOL_HUMAN" | grep -q "unscanned on this volume" || fail "phantom volume --scan must print unscanned: $VOL_HUMAN"
# Error branches on every surface: malformed / unknown scan, wrong volume.
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "x-api-key: $API_KEY" "$BASE/volume?scanId=nope")" = "400" ] || fail "volume scanId must be a UUID"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "x-api-key: $API_KEY" "$BASE/volume?scanId=5e3c1a2b-8d4f-4c6e-9a1b-2f3d4e5f6a7b")" = "404" ] || fail "volume unknown scanId is 404"
mcp_error_text get_volume_status "{\"path\":\"/dev\",\"scanId\":\"$INS_ID\"}" | grep -q 'not' \
  || fail "get_volume_status must refuse a scan from another volume"
echo "e2e: insight — explain, stale, volume and the hidden-space split agree across HTTP, CLI, MCP"

# ---------------------------------------------------------------------------
# 16. Growth (v1.1 Phase 4, phantom-mkn.11): HOT was scanned twice in §11b
# with 3 MiB added in between — two points, a positive slope, a full date.
# Parity on every field that is not a live clock read; the defaults on CLI
# and MCP resolve to the NEWEST completed scan's root (PLANROOT, from §15).
# ---------------------------------------------------------------------------
GR_LIVE='del(.forecast.projectedFullAt, .forecast.daysUntilFull, .forecast.availableBytes)'
GR_HTTP="$(curl -sf -G -H "x-api-key: $API_KEY" --data-urlencode "root=$HOT" "$BASE/scans/series" | jq -Sc "$GR_LIVE")"
GR_CLI="$("$BIN/phantom" growth "$HOT" --json | jq -Sc "$GR_LIVE")"
GR_MCP="$(mcp_result get_growth "{\"root\":\"$HOT\"}" | jq -Sc "$GR_LIVE")"
[ "$GR_CLI" = "$GR_HTTP" ] && [ "$GR_HTTP" = "$GR_MCP" ] || fail "growth: surfaces disagree:
  cli:  $GR_CLI
  http: $GR_HTTP
  mcp:  $GR_MCP"
curl -sf -G -H "x-api-key: $API_KEY" --data-urlencode "root=$HOT" "$BASE/scans/series" | jq -e --arg a "$HOT_ID" --arg b "$HOT2_ID" --arg r "$HOT" '
  .rootPath == $r and .groupBy == "total"
  and (.points | length) >= 2 and .points[-1].scanId == $b and ((.points | map(.scanId) | index($a)) != null)
  and ([.points[].startedAt] == ([.points[].startedAt] | sort))
  and (.series | length) == 1 and .series[0].key == "total" and .series[0].values == [.points[].totalDiskSize]
  and .forecast.method == "linear" and .forecast.pointsUsed == (.points | length)
  and .forecast.bytesPerDay > 0 and .forecast.latestBytes == .points[-1].totalDiskSize
  and .forecast.availableBytes > 0 and .forecast.daysUntilFull >= 0 and (.forecast.projectedFullAt | endswith("Z"))
  and (.forecast.caveat | test("assumes"))' >/dev/null \
  || fail "growth content: $(curl -s -G -H "x-api-key: $API_KEY" --data-urlencode "root=$HOT" "$BASE/scans/series")"
# topLevelDir breaks the root into its child directories with `other` for the
# files directly under it; every line aligns with the points.
curl -sf -G -H "x-api-key: $API_KEY" --data-urlencode "root=$HOT" --data-urlencode "groupBy=topLevelDir" "$BASE/scans/series" \
  | jq -e '.groupBy == "topLevelDir" and (.series | length) >= 1 and (.series | all(.values | length == 2))
    and ([.series[].key] | index("other") != null)' >/dev/null || fail "growth topLevelDir must list child dirs plus other"
# Defaults: no root → the newest completed scan's root, on CLI and MCP alike.
"$BIN/phantom" growth --json | jq -e --arg r "$PLANROOT" '.rootPath == $r' >/dev/null || fail "phantom growth must default to the newest completed scan's root"
mcp_result get_growth '{}' | jq -e --arg r "$PLANROOT" '.rootPath == $r' >/dev/null || fail "get_growth must default to the newest completed scan's root"
mcp_result get_growth '{"groupBy":"extension"}' | jq -e '.groupBy == "extension" and (.series | length) >= 1' >/dev/null || fail "get_growth groupBy extension"
# The human view carries the caveat every time a forecast prints.
GR_HUMAN="$("$BIN/phantom" growth "$HOT")"
echo "$GR_HUMAN" | grep -q "disk full in" || fail "phantom growth must print the forecast: $GR_HUMAN"
echo "$GR_HUMAN" | grep -q "caveat:" || fail "phantom growth must print the caveat beside the forecast: $GR_HUMAN"
# Error branches: unknown root is not found on every surface; the wire
# rejects the CLI's kebab spelling; clap rejects an unknown flag value (2).
[ "$(curl -s -o /dev/null -w '%{http_code}' -G -H "x-api-key: $API_KEY" --data-urlencode "root=/no/such/root" "$BASE/scans/series")" = "404" ] || fail "series unknown root is 404"
[ "$(curl -s -o /dev/null -w '%{http_code}' -G -H "x-api-key: $API_KEY" --data-urlencode "root=$HOT" --data-urlencode "groupBy=top-level-dir" "$BASE/scans/series")" = "400" ] || fail "series rejects kebab groupBy"
mcp_error_text get_growth '{"root":"/no/such/root"}' | grep -qi 'not found' || fail "get_growth unknown root must be not found"
mcp_error_text get_growth "{\"root\":\"$HOT\",\"groupBy\":\"bogus\"}" | grep -q 'groupBy' || fail "get_growth must surface the groupBy error"
set +e
"$BIN/phantom" growth "$HOT" --group-by bogus >/dev/null 2>&1
GR_RC=$?
set -e
[ "$GR_RC" = "2" ] || fail "phantom growth --group-by bogus must exit 2 (got $GR_RC)"
set +e
"$BIN/phantom" growth /no/such/root >/dev/null 2>&1
GR_RC=$?
set -e
[ "$GR_RC" = "3" ] || fail "phantom growth of an unscanned root must exit 3 not-found (got $GR_RC)"
echo "e2e: growth series and forecast agree across HTTP, CLI, MCP"

echo "e2e: PASS"
