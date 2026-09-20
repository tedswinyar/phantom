# Phantom

![Phantom scanning its own repository — treemap, folders tree, and legend](website/static/screenshots/01-main.png)

**A disk-space scanner built to be driven by AI agents.** Every capability —
scan, treemap, per-type breakdown, largest files, reclaimability
classification, scan-to-scan diff — is exposed over a local HTTP API **and**
an MCP server. An agent can drive the entire tool from day one; the Mac app
is one client of that API, not a GUI with scripting bolted on afterward.

"Clean up my Mac" is exactly the chore you want to hand to an agent. That
only works if the agent can reach *everything* the human UI can, and if the
numbers are trustworthy enough to act on and the tool never deletes anything
on its own. Phantom is built for all three.

## Agent-first, not agent-eventually

One local Rust API server owns the scan database; the app, the `phantom`
CLI, and the MCP server are peer clients reading the same persisted results.
Nothing lives only in the GUI.

- **MCP server** — `scan_directory`, `list_scans`, `find_large_files`,
  `get_space_by_type`, `get_treemap`, `get_hotspots`, `explain_path`,
  `find_stale_projects`, `get_volume_status`, `plan_reclaim`,
  `verify_reclaim`, `scan_status`, `cancel_scan`, `diff_scans`, `health` —
  and `skills/phantom-reclaim/SKILL.md` teaches an agent the safe loop:
  scan → plan → confirm → run → verify.
  The full surface, with the same authority the app has.
- **`phantom` CLI** — `--json` on every read, stable exit codes; scriptable
  without a model in the loop.
- **HTTP API** — the contract both of the above speak; documented to the
  byte in `open-prompt-edition/`.

An end-to-end harness asserts the CLI, raw HTTP, and MCP views of the same
scan match **byte-for-byte** on every push — so an agent, a script, and the
app never disagree about a number. Everything is local: the API binds
127.0.0.1 only, key-file auth, zero egress (`docs/threat-model.md`).

## Trustworthy enough for an agent to act on

An agent acting on wrong numbers deletes the wrong things, so the accuracy
and the safety posture are load-bearing, not nice-to-haves:

- **Real occupied size, not apparent size.** `du` reports a
  cloud-dataloaded OneDrive tree as 147 MB while it occupies ~0 physical
  blocks, a hardlinked `~/.cache/uv` as 17 GB that frees only 5, and a
  Finder-duplicated project at twice its footprint (APFS clones report the
  full allocation on every copy). Phantom measures allocated blocks
  everywhere, counts a hardlinked inode or a clone group once, and reports
  `privateSize` beside every `diskSize` — what deleting the thing would
  actually free. Its reclaim estimate is that number, not the listing.
- **Phantom never deletes.** It classifies each hotspot — regenerable build
  artifacts (`target/`, `node_modules`, DerivedData), tool-managed caches,
  cloud placeholders, stale-project artifacts — and surfaces the *safe*
  command (`cargo clean`, `brew cleanup`, `npm cache clean`). You, or your
  agent, decide and execute. There is no destructive operation to
  mis-trigger.
- **Staleness from source, not artifacts.** Project dormancy is judged from
  full-depth *source* mtimes (`cargo sweep` touches `target/` on every run,
  so artifact mtimes lie), and "unknown" is never "stale".

## Install

Requires macOS 14 (Sonoma) or later, Apple Silicon.

**[Download the latest release →](https://github.com/tedswinyar/phantom/releases/latest)**

Open the DMG, drag Phantom to Applications, and launch. It's Developer
ID-signed and notarized, so it opens without Gatekeeper warnings, and it
keeps itself current via Sparkle (EdDSA-signed updates; it asks before
checking).

Or with Homebrew (a tap — the app is the primary artifact, so it cannot live
in homebrew-cask core), which also puts `phantom` and `phantom-mcp` on your
PATH with shell completions and a man page:

```bash
brew install --cask tedswinyar/tap/phantom
```

### Point an agent at it

The MCP server is `Phantom.app/Contents/MacOS/phantom-mcp` (sixteen tools;
the contract is in
[`open-prompt-edition/kit/02-contracts/mcp-protocol.md`](open-prompt-edition/kit/02-contracts/mcp-protocol.md)).
Pick your host:

- **Claude Code** — this repository is a plugin marketplace with one plugin,
  bundling the MCP server registration and the
  [reclaim skill](skills/phantom-reclaim/SKILL.md):
  `/plugin marketplace add tedswinyar/phantom` then
  `/plugin install phantom@phantom`. Or register the server directly:
  `.mcp.json` at the repo root shows the shape.
- **Claude Desktop** — download `phantom-mcp-<version>.mcpb` from the
  release and open it (an MCPB bundle: the server binary plus its manifest).
- **Cursor** — one-click:
  [Add Phantom to Cursor](cursor://anysphere.cursor-deeplink/mcp/install?name=phantom&config=eyJjb21tYW5kIjoiL0FwcGxpY2F0aW9ucy9QaGFudG9tLmFwcC9Db250ZW50cy9NYWNPUy9waGFudG9tLW1jcCIsImFyZ3MiOltdfQ==).
- **Anything else** — the [MCP Registry](https://registry.modelcontextprotocol.io)
  entry is `io.github.tedswinyar/phantom`; any stdio-capable host runs the
  binary directly.

Every host needs Phantom.app installed: the server talks to the app's local
API and never opens the database itself.

Or drive it from a shell:

```bash
phantom scan ~/Code     # CLI (in Phantom.app/Contents/Helpers/phantom)
phantom hotspots        # the reclaimable summary, straight to your terminal
phantom hotspots --json # …the same data an agent or script consumes
phantom plan            # the dry-run plan: safe groups, what each frees
phantom plan --script > plan.sh   # …as a shell script (dry run; PHANTOM_APPLY=1 moves to Trash)
phantom verify <planId> # rescan and report what actually came back
phantom explain <path>  # why THIS path is here, and what deleting it frees
phantom stale --older 6M # projects quiet for six months, with their artifacts
phantom volume          # the data volume's real numbers (not `df /`)
phantom growth ~        # how a root grew across its scans, with the forecast and its caveat
phantom diff --since 7d # what changed since the newest scan at least a week older
phantom completions zsh # shell completions (bash, zsh, fish, …); `phantom man` prints the man page
```

## Build from source

Contributors and Intel Macs (unsupported by the release build) can build the
app locally — see [`docs/getting-started.md`](docs/getting-started.md):

```bash
git clone https://github.com/tedswinyar/phantom.git && cd phantom
make app && open build/Phantom.app
```

## Documentation

| | |
|---|---|
| Tour (clone → app in 10 minutes) | `docs/getting-started.md` |
| The reclaimability taxonomy and measurement rules | `docs/reclaimability.md` |
| Architecture (one hub, thin clients) | `docs/adr/0001-constellation-architecture.md` |
| Wire contract + rebuild kit (Open Prompt Edition) | `open-prompt-edition/` |
| Threat model / security posture | `docs/threat-model.md`, `SECURITY.md` |

## License

MIT — see `LICENSE`. Third-party attributions: `THIRD-PARTY-NOTICES.html`.
