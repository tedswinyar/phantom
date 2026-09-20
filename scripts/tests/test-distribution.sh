#!/usr/bin/env bash
set -u

# Tests OF the distribution renderers (scripts/lib/distribution.sh): the
# cask, the MCPB manifest + bundle, the registry server.json and the Cursor
# deeplink, from fixed inputs — no network, no gh, no keychain.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../lib/distribution.sh
. "$SCRIPT_DIR/lib/distribution.sh"

PASS=0
FAIL=0
t() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "  ✗ $desc" >&2; fi
}
command -v jq >/dev/null || { echo "test-distribution: jq is required" >&2; exit 1; }

V="9.9.9"
DMG_URL="https://github.com/tedswinyar/phantom/releases/download/v9.9.9/Phantom-9.9.9.dmg"
MCPB_URL="https://github.com/tedswinyar/phantom/releases/download/v9.9.9/phantom-mcp-9.9.9.mcpb"
SHA="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
TMP="$(mktemp -d /tmp/phantom-dist-test.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# --- cask -------------------------------------------------------------------
render_cask "$V" "$DMG_URL" "$SHA" > "$TMP/cask.rb"
t "cask declares the version and sha256 it was given" \
  bash -c "grep -q 'version \"$V\"' '$TMP/cask.rb' && grep -q 'sha256 \"$SHA\"' '$TMP/cask.rb'"
t "cask url is the release asset" \
  grep -qF "url \"$DMG_URL\"" "$TMP/cask.rb"
t "cask installs the app and symlinks BOTH binaries (CLI in Helpers, MCP in MacOS)" \
  bash -c "grep -q 'app \"Phantom.app\"' '$TMP/cask.rb' && grep -q 'Helpers/phantom\"' '$TMP/cask.rb' && grep -q 'MacOS/phantom-mcp\"' '$TMP/cask.rb'"
t "cask ships completions for bash, zsh and fish plus the man page" \
  bash -c "grep -q bash_completion '$TMP/cask.rb' && grep -q zsh_completion '$TMP/cask.rb' && grep -q fish_completion '$TMP/cask.rb' && grep -q 'manpage .*man1/phantom.1' '$TMP/cask.rb'"
t "cask requires Sonoma and arm64 (the release build's floor)" \
  bash -c "grep -q ':sonoma' '$TMP/cask.rb' && grep -q 'arch: :arm64' '$TMP/cask.rb'"
t "cask zap trashes only Phantom's own directories" \
  bash -c "! sed -n '/zap trash/,/\\]/p' '$TMP/cask.rb' | grep '\"' | grep -viE 'phantom'"

# --- MCPB manifest + bundle ---------------------------------------------------
render_mcpb_manifest "$V" > "$TMP/manifest.json"
t "mcpb manifest is JSON at manifest_version 0.3 with the given version" \
  jq -e ".manifest_version == \"0.3\" and .version == \"$V\" and .name == \"phantom\"" "$TMP/manifest.json"
t "mcpb manifest is a binary server whose command is the bundled phantom-mcp under \${__dirname}" \
  jq -e '.server.type == "binary" and .server.entry_point == "server/phantom-mcp" and .server.mcp_config.command == "${__dirname}/server/phantom-mcp" and .server.mcp_config.args == []' "$TMP/manifest.json"
t "mcpb manifest is darwin-only" \
  jq -e '.compatibility.platforms == ["darwin"]' "$TMP/manifest.json"

FAKE_APP="$TMP/Phantom.app"
mkdir -p "$FAKE_APP/Contents/MacOS" "$FAKE_APP/Contents/Resources"
printf '#!/bin/sh\necho fake\n' > "$FAKE_APP/Contents/MacOS/phantom-mcp"; chmod +x "$FAKE_APP/Contents/MacOS/phantom-mcp"
printf 'icon' > "$FAKE_APP/Contents/Resources/AppIcon.icns"
OUT="$TMP/out.mcpb"
# The library's functions live in THIS shell; run them here and let `t`
# check the artifacts (a `bash -c` child would not have them).
build_mcpb "$FAKE_APP" "$V" "$OUT" >/dev/null 2>&1; BUILD_RC=$?
unzip -Z1 "$OUT" > "$TMP/list" 2>/dev/null || true
t "build_mcpb produces a zip holding manifest.json, server/phantom-mcp and the icon — and no __MACOSX sidecars" \
  bash -c "[ '$BUILD_RC' = 0 ] && grep -qx 'manifest.json' '$TMP/list' && grep -qx 'server/phantom-mcp' '$TMP/list' && grep -qx 'icon.icns' '$TMP/list' && ! grep -q '__MACOSX' '$TMP/list'"
t "build_mcpb keeps the server executable and the manifest's version" \
  bash -c "unzip -q '$OUT' -d '$TMP/unpacked' && [ -x '$TMP/unpacked/server/phantom-mcp' ] && jq -e '.version == \"$V\"' '$TMP/unpacked/manifest.json'"
if build_mcpb "$TMP" "$V" "$OUT.2" >/dev/null 2>&1; then REFUSED=0; else REFUSED=1; fi
t "build_mcpb refuses an app bundle without phantom-mcp" \
  [ "$REFUSED" = 1 ]

# --- registry server.json -------------------------------------------------------
render_server_json "$V" "$MCPB_URL" "$SHA" > "$TMP/server.json"
t "server.json names io.github.tedswinyar/phantom at the given version" \
  jq -e ".name == \"io.github.tedswinyar/phantom\" and .version == \"$V\"" "$TMP/server.json"
t "server.json carries one mcpb package: the asset URL (containing mcp), its sha256, stdio" \
  jq -e "(.packages | length) == 1 and .packages[0].registryType == \"mcpb\" and .packages[0].identifier == \"$MCPB_URL\" and (.packages[0].identifier | test(\"mcp\")) and .packages[0].fileSha256 == \"$SHA\" and .packages[0].transport.type == \"stdio\"" "$TMP/server.json"
render_server_json "$V" "$MCPB_URL" "$SHA" "someone/elsewhere" > "$TMP/server2.json"
t "server.json points at the given repository" \
  jq -e '.repository.url == "https://github.com/someone/elsewhere"' "$TMP/server2.json"

# --- Cursor deeplink --------------------------------------------------------------
cursor_deeplink > "$TMP/link.txt"
t "cursor deeplink names phantom and base64-encodes the default command" \
  bash -c "grep -q '^cursor://anysphere.cursor-deeplink/mcp/install?name=phantom&config=' '$TMP/link.txt' && sed 's/.*config=//' '$TMP/link.txt' | base64 -d | jq -e '.command == \"/Applications/Phantom.app/Contents/MacOS/phantom-mcp\" and .args == []'"

# --- the repo IS the plugin -----------------------------------------------------------
ROOT="$(dirname "$SCRIPT_DIR")"
t "plugin.json and marketplace.json agree on name and version, and the marketplace source is the repo root" \
  bash -c "[ \"\$(jq -r .name '$ROOT/.claude-plugin/plugin.json')\" = phantom ] && [ \"\$(jq -r .version '$ROOT/.claude-plugin/plugin.json')\" = \"\$(jq -r '.plugins[0].version' '$ROOT/.claude-plugin/marketplace.json')\" ] && [ \"\$(jq -r '.plugins[0].source' '$ROOT/.claude-plugin/marketplace.json')\" = './' ]"
t "plugin version matches the OPE contract VERSION (one release, one number)" \
  bash -c "[ \"\$(jq -r .version '$ROOT/.claude-plugin/plugin.json')\" = \"\$(tr -d '[:space:]' < '$ROOT/open-prompt-edition/VERSION')\" ]"
t "the plugin's MCP config and skill exist at the plugin root" \
  bash -c "jq -e '.mcpServers.phantom.command' '$ROOT/.mcp.json' && [ -f '$ROOT/skills/phantom-reclaim/SKILL.md' ]"

echo "test-distribution: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
