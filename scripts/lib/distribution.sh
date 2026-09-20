#!/usr/bin/env bash
# distribution.sh — pure renderers for the distribution artifacts that ride
# beside the DMG (v1.1 Phase 6, phantom-mkn.18): the Homebrew cask, the MCP
# Registry server.json, the MCPB bundle for Claude Desktop, and the Cursor
# deeplink. No network, no gh, no keychain — publish-release.sh feeds them
# the version, URLs and digests; scripts/tests/test-distribution.sh feeds
# them fixtures. Every function prints to stdout.
#
# Nothing here publishes: pushing the cask to the tap, submitting server.json
# to the registry and signing the MCPB are steps a human runs (see
# docs/build-pipeline.md "Distribution artifacts").

PUBLIC_REPO_DEFAULT="tedswinyar/phantom"

# render_cask <version> <dmg-url> <dmg-sha256>
# Homebrew cask for a tap (core rejects .app-primary formulae). `binary`
# symlinks the CLI and the MCP server into brew's bin; completions and the
# man page come from the paths build-app.sh generates inside the bundle.
render_cask() {
  local version="$1" url="$2" sha="$3"
  cat <<CASK
cask "phantom" do
  version "$version"
  sha256 "$sha"

  url "$url"
  name "Phantom"
  desc "Disk-usage analyzer for macOS with honest physical sizes, tiered reclaim suggestions and an MCP server"
  homepage "https://github.com/tedswinyar/phantom"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :sonoma"
  depends_on arch: :arm64

  app "Phantom.app"
  binary "#{appdir}/Phantom.app/Contents/Helpers/phantom"
  binary "#{appdir}/Phantom.app/Contents/MacOS/phantom-mcp"
  bash_completion "#{appdir}/Phantom.app/Contents/Resources/completions/phantom.bash"
  zsh_completion "#{appdir}/Phantom.app/Contents/Resources/completions/_phantom"
  fish_completion "#{appdir}/Phantom.app/Contents/Resources/completions/phantom.fish"
  manpage "#{appdir}/Phantom.app/Contents/Resources/man/man1/phantom.1"

  zap trash: [
    "~/Library/Application Support/phantom",
    "~/Library/Logs/Phantom",
    "~/Library/Preferences/com.tedswinyar.phantom.plist",
    "~/Library/Caches/com.tedswinyar.phantom",
  ]
end
CASK
}

# render_mcpb_manifest <version>
# manifest.json for the MCPB bundle: a `binary` server whose entry point is
# the bundled phantom-mcp. The server reads PHANTOM_* like every client;
# nothing here needs user_config — the API's key file is found by convention.
render_mcpb_manifest() {
  local version="$1"
  cat <<MANIFEST
{
  "manifest_version": "0.3",
  "name": "phantom",
  "display_name": "Phantom",
  "version": "$version",
  "description": "Disk-usage analysis for macOS an agent can query: honest physical sizes, tiered reclaim suggestions, scan → plan → verify. Never deletes.",
  "long_description": "Phantom's MCP server (sixteen tools). Requires Phantom.app running or installed: the server talks HTTP to the app's API on 127.0.0.1:18770 and reads the key file it writes under ~/Library/Application Support/phantom.",
  "author": { "name": "Ted Swinyar", "url": "https://github.com/tedswinyar" },
  "homepage": "https://github.com/tedswinyar/phantom",
  "documentation": "https://github.com/tedswinyar/phantom/blob/main/open-prompt-edition/kit/02-contracts/mcp-protocol.md",
  "license": "MIT",
  "keywords": ["disk", "storage", "macos", "cleanup"],
  "server": {
    "type": "binary",
    "entry_point": "server/phantom-mcp",
    "mcp_config": {
      "command": "\${__dirname}/server/phantom-mcp",
      "args": [],
      "env": {}
    }
  },
  "compatibility": {
    "platforms": ["darwin"]
  }
}
MANIFEST
}

# build_mcpb <app-dir> <version> <out.mcpb>
# An MCPB is a zip: manifest.json at the root plus the server binary. Built
# with ditto so the binary's permissions survive. Unsigned:
# `mcpb sign` is the human step (docs/build-pipeline.md).
build_mcpb() {
  local app_dir="$1" version="$2" out="$3" stage
  [ -x "$app_dir/Contents/MacOS/phantom-mcp" ] || { echo "build_mcpb: no phantom-mcp in $app_dir" >&2; return 1; }
  stage="$(mktemp -d /tmp/phantom-mcpb.XXXXXX)"
  mkdir -p "$stage/server"
  cp "$app_dir/Contents/MacOS/phantom-mcp" "$stage/server/phantom-mcp"
  render_mcpb_manifest "$version" > "$stage/manifest.json"
  if [ -f "$app_dir/Contents/Resources/AppIcon.icns" ]; then
    cp "$app_dir/Contents/Resources/AppIcon.icns" "$stage/icon.icns"
  fi
  rm -f "$out"
  # --norsrc: no __MACOSX/ resource-fork sidecars in the bundle (the
  # binary's code signature lives in the Mach-O itself, not a fork).
  (cd "$stage" && ditto -c -k --norsrc . "$out")
  rm -rf "$stage"
  [ -s "$out" ]
}

# render_server_json <version> <mcpb-url> <mcpb-sha256> [public-repo]
# MCP Registry entry (registryType mcpb; the identifier is the release
# asset URL and must contain "mcp", which "phantom-mcp" does).
render_server_json() {
  local version="$1" url="$2" sha="$3" repo="${4:-$PUBLIC_REPO_DEFAULT}"
  cat <<SERVERJSON
{
  "\$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
  "name": "io.github.tedswinyar/phantom",
  "title": "Phantom",
  "description": "Disk-usage analysis for macOS an agent can query: honest physical sizes, tiered reclaim suggestions, scan → plan → verify. Never deletes.",
  "version": "$version",
  "repository": {
    "url": "https://github.com/$repo",
    "source": "github"
  },
  "websiteUrl": "https://github.com/$repo",
  "packages": [
    {
      "registryType": "mcpb",
      "identifier": "$url",
      "fileSha256": "$sha",
      "transport": { "type": "stdio" }
    }
  ]
}
SERVERJSON
}

# cursor_deeplink [command-path]
# cursor://anysphere.cursor-deeplink/mcp/install?name=phantom&config=<base64 of the server config>
cursor_deeplink() {
  local cmd="${1:-/Applications/Phantom.app/Contents/MacOS/phantom-mcp}" config
  config="$(printf '{"command":"%s","args":[]}' "$cmd" | base64 | tr -d '\n')"
  printf 'cursor://anysphere.cursor-deeplink/mcp/install?name=phantom&config=%s\n' "$config"
}

# sha256_of <file>
sha256_of() {
  shasum -a 256 "$1" | cut -d' ' -f1
}
