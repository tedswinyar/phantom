#!/usr/bin/env bash
set -u

# OPE conformance harness — black-box wire-format checks against a LIVE
# implementation over HTTP. Any implementation (any language) that passes
# this is wire-compatible with the contract in kit/02, 03, 04, 06.
#
# Usage:
#   run.sh                 # boots the reference implementation on a temp DB
#   run.sh <base-url> <api-key>   # tests an already-running implementation
#
# SAFETY: refuses to run against the production port — conformance creates
# and mutates data.

ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
FIXTURES="$ROOT_DIR/tests/fixtures"
# Refuse the whole prod port LADDER (base .. base+9), not just the base:
# the API climbs the ladder when the base is busy, so a prod server may be
# listening anywhere in this range. init.sh rewrites 8768 to the stamped
# project's real prod port, so this stays correct after stamping.
PROD_PORT=8768
PROD_PORT_MAX=18309

PASS=0
FAIL=0
API_PID=""
WORK=""

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

cleanup() {
  if [ -n "$API_PID" ]; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  [ -n "$WORK" ] && rm -rf "$WORK"
  [ -n "${SCAN_DIR:-}" ] && rm -rf "$SCAN_DIR"
}
trap cleanup EXIT

command -v jq >/dev/null || { echo "conformance: jq is required" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Target resolution
# ---------------------------------------------------------------------------
if [ $# -ge 1 ]; then
  BASE="$1"
  KEY="${2:-}"
else
  WORK="$(mktemp -d /tmp/phantom-conformance.XXXXXX)"
  BIN="$ROOT_DIR/rust/target/debug/phantom-api"
  if [ ! -x "$BIN" ]; then
    (cd "$ROOT_DIR/rust" && cargo build -p phantom-api --quiet) || exit 1
  fi
  PHANTOM_PROFILE=test \
  PHANTOM_DB_PATH="$WORK/conformance.db" \
  PHANTOM_PORT=0 \
  PHANTOM_KEY_FILE="$WORK/api_key" \
    "$BIN" >"$WORK/api.out" 2>"$WORK/api.err" &
  API_PID=$!
  BASE=""
  for _ in $(seq 1 50); do
    BASE="$(sed -n 's/.*listening on //p' "$WORK/api.out")"
    [ -n "$BASE" ] && break
    sleep 0.1
  done
  [ -n "$BASE" ] || { echo "conformance: reference API never announced" >&2; exit 1; }
  KEY="$(cat "$WORK/api_key")"
fi

BASE_PORT="$(printf '%s' "$BASE" | sed -n 's|.*:\([0-9][0-9]*\).*|\1|p')"
if [ -n "$BASE_PORT" ] && [ "$BASE_PORT" -ge "$PROD_PORT" ] && [ "$BASE_PORT" -le "$PROD_PORT_MAX" ]; then
  echo "conformance: REFUSING to run against the production port range ($PROD_PORT-$PROD_PORT_MAX); got $BASE_PORT" >&2
  echo "conformance: this harness creates and mutates data" >&2
  exit 1
fi

api() {
  # api <method> <path> [json-body]
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -s -X "$method" -H "x-api-key: $KEY" -H "content-type: application/json" \
      -d "$body" "$BASE$path"
  else
    curl -s -X "$method" -H "x-api-key: $KEY" "$BASE$path"
  fi
}

api_status() {
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -s -o /dev/null -w '%{http_code}' -X "$method" -H "x-api-key: $KEY" \
      -H "content-type: application/json" -d "$body" "$BASE$path"
  else
    curl -s -o /dev/null -w '%{http_code}' -X "$method" -H "x-api-key: $KEY" "$BASE$path"
  fi
}

echo "conformance: target $BASE"

# ---------------------------------------------------------------------------
# Health + auth boundary
# ---------------------------------------------------------------------------
t "health is open and ok" \
  bash -c "curl -s '$BASE/health' | jq -e '.status == \"ok\"'"
t "scans require the api key" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' '$BASE/scans') = 401 ]"
t "wrong key is 401" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: wrong' '$BASE/scans') = 401 ]"

# ---------------------------------------------------------------------------
# Create: full wire shape (Rule 2: the round-trip is the contract). The scan
# walks a tiny deterministic tree created here — this product scans the disk
# the implementation runs on, so conformance runs on the same host. Two
# persisted-size files so the files listing has two rows to paginate.
# ---------------------------------------------------------------------------
SCAN_DIR="$(mktemp -d /tmp/phantom-conformance-tree.XXXXXX)"
perl -e 'print "\x01" x (1024*1024)' > "$SCAN_DIR/one.bin"
perl -e 'print "\x02" x (1024*1024)' > "$SCAN_DIR/two.bin"
# One hotspot, so the hotspots section below has a non-empty group to check.
# No package.json at the root: no project-root/dormancy math, so the
# category is deterministically regenerableArtifact regardless of run date.
mkdir "$SCAN_DIR/node_modules"
perl -e 'print "\x03" x (1024*1024)' > "$SCAN_DIR/node_modules/chunk.bin"
# A sub-1MiB file beside chunk.bin: its row is filtered but its bytes stay in
# node_modules' aggregate, so the treemap section below gets a residual tile
# (300 KiB ≈ 22% of the dir — far above the 0.5% threshold).
perl -e 'print "\x04" x (300*1024)' > "$SCAN_DIR/node_modules/tiny.js"
# A second hardlink to one.bin. Named to sort LAST (the walk is sorted, the
# FIRST link is charged): one.bin keeps its persisted row; this link is an
# entry (fileCount) but adds no bytes and no row — the dedup checks below.
ln "$SCAN_DIR/one.bin" "$SCAN_DIR/zz-linked.bin"

CREATED="$(api POST /scans "{\"rootPath\":\"$SCAN_DIR\"}")"
ID="$(printf '%s' "$CREATED" | jq -r .id)"

t "create answers 202 accepted" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'x-api-key: $KEY' -H 'content-type: application/json' -d '{\"rootPath\":\"$SCAN_DIR\"}' '$BASE/scans') = 202 ]"
t "create returns all twelve wire keys" \
  bash -c "printf '%s' '$CREATED' | jq -e 'has(\"id\") and has(\"rootPath\") and has(\"status\") and has(\"startedAt\") and has(\"finishedAt\") and has(\"totalDiskSize\") and has(\"totalLogicalSize\") and has(\"fileCount\") and has(\"dirCount\") and has(\"errorCount\") and has(\"unreadablePaths\") and has(\"progress\")'"
t "startedAt is canonical 6-digit Z form" \
  bash -c "printf '%s' '$CREATED' | jq -r .startedAt | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'"
t "id is lowercase hyphenated uuid" \
  bash -c "printf '%s' '$ID' | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'"

# Poll to terminal: progress is an object while running, null once terminal
# (nullable-present-as-null); finishedAt fills in canonically.
TERMINAL=""
for _ in $(seq 1 100); do
  TERMINAL="$(api GET "/scans/$ID")"
  [ "$(printf '%s' "$TERMINAL" | jq -r .status)" != "running" ] && break
  sleep 0.1
done
t "scan reaches the complete status" \
  bash -c "printf '%s' '$TERMINAL' | jq -e '.status == \"complete\"'"
t "terminal progress is present-as-null" \
  bash -c "printf '%s' '$TERMINAL' | jq -e 'has(\"progress\") and .progress == null'"
t "finishedAt is canonical 6-digit Z form" \
  bash -c "printf '%s' '$TERMINAL' | jq -r .finishedAt | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'"

t "uppercase uuid in path is accepted (Rule: any case in)" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/'\$(printf '%s' '$ID' | tr a-f A-F)) = 200 ]"

# ---------------------------------------------------------------------------
# Validation + errors
# ---------------------------------------------------------------------------
t "empty rootPath is 400 with error shape" \
  bash -c "api() { curl -s -X POST -H 'x-api-key: $KEY' -H 'content-type: application/json' -d '{\"rootPath\":\"   \"}' '$BASE/scans'; }; api | jq -e 'has(\"error\")'"
t "unknown fields are rejected (422)" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'x-api-key: $KEY' -H 'content-type: application/json' -d '{\"rootPath\":\"/tmp\",\"maxDepth\":3}' '$BASE/scans') = 422 ]"
t "unknown-field rejection is {error}-shaped (all non-2xx bodies are {error})" \
  bash -c "curl -s -X POST -H 'x-api-key: $KEY' -H 'content-type: application/json' -d '{\"rootPath\":\"/tmp\",\"maxDepth\":3}' '$BASE/scans' | jq -e 'has(\"error\")'"
t "unknown uuid is 404" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/00000000-0000-0000-0000-000000000000') = 404 ]"
t "malformed uuid is 400" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/not-a-uuid') = 400 ]"
t "malformed-uuid rejection is {error}-shaped" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/not-a-uuid' | jq -e 'has(\"error\")'"
t "unknown query key on a results route is 400" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?filetype=rs') = 400 ]"
t "unknown-key rejection is {error}-shaped and names the field" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?filetype=rs' | jq -e '.error | test(\"filetype\")'"

# ---------------------------------------------------------------------------
# State transition: cancel of a terminal scan is a 409 conflict, {error}-shaped
# ---------------------------------------------------------------------------
t "cancel of a terminal scan is 409" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'x-api-key: $KEY' '$BASE/scans/$ID/cancel') = 409 ]"
t "cancel conflict is {error}-shaped" \
  bash -c "curl -s -X POST -H 'x-api-key: $KEY' '$BASE/scans/$ID/cancel' | jq -e 'has(\"error\")'"

# ---------------------------------------------------------------------------
# List shape
# ---------------------------------------------------------------------------
t "scan list is an array containing the scan" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans' | jq -e --arg id '$ID' 'type == \"array\" and any(.[]; .id == \$id)'"

# ---------------------------------------------------------------------------
# Pagination: ?limit bounds the files page (body stays a bare array), and the
# continuation token rides the X-Next-Cursor response header when more remain.
# Following the cursor returns the next page. No params = first page.
# ---------------------------------------------------------------------------
t "?limit=1 returns a single-element array" \
  bash -c "[ \$(curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?limit=1' | jq 'length') = 1 ]"
t "?limit=1 sets X-Next-Cursor when more remain" \
  bash -c "curl -s -D - -o /dev/null -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?limit=1' | grep -qi '^x-next-cursor:'"
t "following the cursor returns a further array" \
  bash -c "c=\$(curl -s -D - -o /dev/null -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?limit=1' | sed -n 's/^[Xx]-[Nn]ext-[Cc]ursor: *//p' | tr -d '\r'); curl -s -H 'x-api-key: $KEY' \"$BASE/scans/$ID/files?limit=1&cursor=\$c\" | jq -e 'type == \"array\" and length >= 1'"
t "a bad limit is 400 with error shape" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/files?limit=nope' | jq -e 'has(\"error\")'"

# ---------------------------------------------------------------------------
# Fixture agreement (Rule 4 lite): our decoder-of-record already runs in the
# unit suites; here assert the fixture files still match the live shape.
# ---------------------------------------------------------------------------
t "live scan has exactly the fixture's key set" \
  bash -c "diff <(curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID' | jq -S 'keys') <(jq -S 'keys' '$FIXTURES/scan-complete.json')"
t "live entry has exactly the fixture's key set" \
  bash -c "diff <(curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/entry?path=$SCAN_DIR/one.bin' | jq -S 'keys') <(jq -S 'keys' '$FIXTURES/entry.json')"

# ---------------------------------------------------------------------------
# Per-dir descendant counts: aggregated from the FULL walk (schema v3). The
# tree has 5 files (tiny.js's row is filtered yet it counts; zz-linked.bin
# is an entry even though its bytes are deduped) and 1 directory below the
# root; file rows carry the counts present-as-null.
# ---------------------------------------------------------------------------
t "root dir entry carries full-walk descendant counts" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/entry?path=$SCAN_DIR' | jq -e '.fileCount == 5 and .dirCount == 1'"
t "file entry carries counts present-as-null" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/entry?path=$SCAN_DIR/one.bin' | jq -e 'has(\"fileCount\") and .fileCount == null and has(\"dirCount\") and .dirCount == null'"

# ---------------------------------------------------------------------------
# Hardlink dedup (kit 0.9.1): zz-linked.bin shares one.bin's inode. Naively
# the tree sums past 4.3 MiB; deduped it is ~3.3 MiB. The threshold between
# the two (4 MiB) discriminates: an implementation that charges an inode
# more than once per scan fails here.
# ---------------------------------------------------------------------------
# unreadablePaths (kit 0.9.2): recorded-and-empty on a clean complete scan
# — an ARRAY, not null (null is reserved for pre-v4 rows).
t "complete scan carries an unreadablePaths array" \
  bash -c "printf '%s' '$TERMINAL' | jq -e '.unreadablePaths | type == \"array\"'"

t "totalDiskSize counts a hardlinked inode once" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID' | jq -e '.totalDiskSize < 4194304 and .totalDiskSize >= 3145728'"
t "the duplicate link has no persisted row of its own" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/files' | jq -e '[.[].name] | index(\"zz-linked.bin\") == null and index(\"one.bin\") != null'"

# ---------------------------------------------------------------------------
# Types: bare array whose rows match the shared fixture's key set exactly.
# ---------------------------------------------------------------------------
t "types is a bare array" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/types' | jq -e 'type == \"array\" and length >= 1'"
t "live type total has exactly the fixture's key set" \
  bash -c "diff <(curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/types' | jq -S '.[0] | keys') <(jq -S '.[0] | keys' '$FIXTURES/types.json')"

# ---------------------------------------------------------------------------
# Hotspots: classified once at completion, persisted; an OBJECT, not an
# array; 409 while running; empty summary when a terminal scan stored none.
# ---------------------------------------------------------------------------
HOTSPOTS="$(api GET "/scans/$ID/hotspots")"
t "hotspots summary has exactly the fixture's key set" \
  bash -c "diff <(printf '%s' '$HOTSPOTS' | jq -S 'keys') <(jq -S 'keys' '$FIXTURES/hotspots-summary.json')"
t "hotspot group has exactly the fixture group's key set" \
  bash -c "diff <(printf '%s' '$HOTSPOTS' | jq -S '.groups[0] | keys') <(jq -S '.groups[0] | keys' '$FIXTURES/hotspots-summary.json')"
t "group command is present and null-or-string (node_modules: null — its cleanup is deletion, never a suggested command)" \
  bash -c "printf '%s' '$HOTSPOTS' | jq -e '.groups[0] | has(\"command\") and .command == null'"
t "node_modules classified as a regenerable hotspot" \
  bash -c "printf '%s' '$HOTSPOTS' | jq -e '.groups[0].ruleId == \"node-modules\" and .groups[0].category == \"regenerableArtifact\"'"
t "reclaim estimate equals the deduped group size" \
  bash -c "printf '%s' '$HOTSPOTS' | jq -e '.reclaimEstimate == .groups[0].diskSize and .groups[0].diskSize >= 1048576'"
t "hotspot directory row carries its category" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/entry?path=$SCAN_DIR/node_modules' | jq -e '.isDir == true and .category == \"regenerableArtifact\"'"
t "uncategorized entry carries category present-as-null" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/entry?path=$SCAN_DIR/one.bin' | jq -e 'has(\"category\") and .category == null'"

# ---------------------------------------------------------------------------
# Treemap: rect key set vs the fixture, and the residual pseudo-tile — a dir
# whose persisted children under-sum its aggregate (node_modules: chunk.bin
# + the filtered tiny.js) must yield exactly one residual rect carrying the
# PARENT's path, "smaller files", and no children of its own.
# ---------------------------------------------------------------------------
t "live treemap rect has exactly the fixture's key set" \
  bash -c "diff <(curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/treemap' | jq -S '.rects[0] | keys') <(jq -S '.rects[0] | keys' '$FIXTURES/treemap.json')"
t "under-summed dir yields one residual tile resolving to the parent" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/treemap' | jq -e --arg nm '$SCAN_DIR/node_modules' '[.rects[] | select(.residual)] | length == 1 and .[0].path == \$nm and .[0].name == \"smaller files\" and .[0].isDir == false and .[0].fileType == null'"
t "real rects carry residual: false, present" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID/treemap' | jq -e '[.rects[] | select(.residual | not)] | length >= 4 and all(.[]; .residual == false)'"

# The 409 gate needs a scan that is verifiably still running when we ask.
# Black-box, so no test hook: use a tree big enough (20k files) that the
# walk takes orders of magnitude longer than one loopback round-trip.
BIG_DIR="$(mktemp -d /tmp/phantom-conformance-big.XXXXXX)"
perl -e 'for $d (1..40) { mkdir "$ARGV[0]/d$d"; for $f (1..500) { open my $fh, ">", "$ARGV[0]/d$d/f$f" or die; close $fh; } }' "$BIG_DIR"
BIG_ID="$(api POST /scans "{\"rootPath\":\"$BIG_DIR\"}" | jq -r .id)"
t "hotspots while the scan runs is 409" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/$BIG_ID/hotspots') = 409 ]"
t "the running-scan conflict is {error}-shaped" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$BIG_ID/hotspots' | jq -e 'has(\"error\")'"
for _ in $(seq 1 200); do
  [ "$(api GET "/scans/$BIG_ID" | jq -r .status)" != "running" ] && break
  sleep 0.1
done
t "a completed scan with no hotspots serves the empty summary" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$BIG_ID/hotspots' | jq -e '.groups == [] and .reclaimEstimate == 0 and .reviewDiskSize == 0'"
rm -rf "$BIG_DIR"

# ---------------------------------------------------------------------------
# Diff (phantom-081): a second scan of the SAME root, then diff old->new.
# The tree is unchanged between scans, so every delta is 0 and the movement
# lists are empty — but the shape, the echoed ids, and the exactness of the
# zero are the contract. A cross-root diff is a 400, not a 409.
# ---------------------------------------------------------------------------
ID2="$(api POST /scans "{\"rootPath\":\"$SCAN_DIR\"}" | jq -r .id)"
for _ in $(seq 1 200); do
  [ "$(api GET "/scans/$ID2" | jq -r .status)" != "running" ] && break
  sleep 0.1
done
DIFF="$(api GET "/scans/$ID/diff/$ID2")"
t "diff carries the full key set and echoes both scan ids" \
  bash -c "printf '%s' '$DIFF' | jq -e 'has(\"scanA\") and has(\"scanB\") and has(\"scanAStartedAt\") and has(\"scanBStartedAt\") and has(\"reversedChronology\") and has(\"rootPath\") and has(\"diskDelta\") and has(\"logicalDelta\") and has(\"fileCountDelta\") and has(\"dirCountDelta\") and has(\"errorCountDelta\") and has(\"grown\") and has(\"freed\") and .scanA == \"$ID\" and .scanB == \"$ID2\"'"
t "an unchanged tree diffs to exact zero with empty movement lists" \
  bash -c "printf '%s' '$DIFF' | jq -e '.diskDelta == 0 and .fileCountDelta == 0 and (.grown | length == 0) and (.freed | length == 0)'"
t "natural order (older scanA) leaves reversedChronology null" \
  bash -c "printf '%s' '$DIFF' | jq -e '.reversedChronology == null and (.scanAStartedAt <= .scanBStartedAt)'"
t "reverse order (newer scanA) sets reversedChronology true" \
  bash -c "curl -s -H 'x-api-key: $KEY' '$BASE/scans/$ID2/diff/$ID' | jq -e '.reversedChronology == true'"
t "diff across different roots is 400" \
  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'x-api-key: $KEY' '$BASE/scans/$ID/diff/$BIG_ID') = 400 ]"

# ---------------------------------------------------------------------------
# Schema drift (self-boot mode only — an external target's database is not
# ours to open): apply kit/03-schema/schema.sql to a fresh database and
# compare its NORMALIZED structure (tables, columns, user_version) against
# the reference database the booted API created. Textual .schema diffs would
# trip on comments and on CREATE-vs-ALTER formatting; structure cannot.
# ---------------------------------------------------------------------------
if [ -n "$WORK" ]; then
  if command -v sqlite3 >/dev/null; then
    schema_shape() { # db-path
      sqlite3 "$1" "SELECT name FROM sqlite_master
                    WHERE type IN ('table','index') AND name NOT LIKE 'sqlite_%'
                    ORDER BY name;"
      sqlite3 "$1" "SELECT name FROM sqlite_master
                    WHERE type='table' AND name NOT LIKE 'sqlite_%'
                    ORDER BY name;" | while read -r tbl; do
        echo "-- $tbl"
        sqlite3 "$1" "PRAGMA table_info($tbl);"
      done
      echo "user_version=$(sqlite3 "$1" 'PRAGMA user_version;')"
    }
    sqlite3 "$WORK/from-kit.db" < "$ROOT_DIR/open-prompt-edition/kit/03-schema/schema.sql"
    schema_shape "$WORK/from-kit.db" > "$WORK/shape-kit.txt"
    schema_shape "$WORK/conformance.db" > "$WORK/shape-live.txt"
    t "kit schema.sql matches the reference database structure" \
      diff "$WORK/shape-kit.txt" "$WORK/shape-live.txt"
  else
    echo "conformance: sqlite3 not found; skipping the schema drift check" >&2
  fi
fi

echo "conformance: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
