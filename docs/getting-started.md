# Getting started — clone to running app in ~10 minutes

This is the human tour.

## Prerequisites

- macOS 14+, Xcode command-line tools (`swift --version` works)
- Rust (`cargo --version`)
- `jq`, `hugo` (website layer only), `bd` (beads, optional but assumed)

## 1. Build and test everything

```bash
./scripts/verify.sh
```

One command, every suite: Rust unit + integration, Swift unit, an e2e parity
harness, the scripts suite, OPE conformance, website build. Layers that were
pruned at stamp time are skipped automatically. Expect the first run to take
a few minutes (fresh cargo build); later runs are incremental.

## 2. Run the app

```bash
make app                 # assemble build/Phantom.app (ad-hoc signed)
open build/Phantom.app
```

The app starts its own bundled API server. Prefer a seeded playground?

```bash
./scripts/dev-build.sh   # debug build + sample data + launch (dev profile)
```

## 3. Poke it from the command line

```bash
./scripts/start.sh &                       # dev-profile API (port 8778)
export PHANTOM_PROFILE=dev              # Point CLI at dev profile too
cd rust
./target/debug/phantom note create "Hello" --body "from the CLI"
./target/debug/phantom note list --json
```

**Note**: `start.sh` runs the **dev** profile (port 8778), so the CLI needs
`PHANTOM_PROFILE=dev` to match. Without it, the CLI defaults to prod (port
8768) and can't reach the server.

The CLI, the MCP server (`phantom-mcp`, wired in `.mcp.json`), and the app
are three clients of the same API — creating a note in one shows up in all.

## 4. Install the push gate

```bash
./scripts/install-hooks.sh   # pre-push hook: check-hooks + verify.sh
```

Every branch push now runs the full verify gate. `git push` on a red suite is
blocked by design.

## 5. Where things are

| Want to… | Look at |
|---|---|
| Understand the architecture | `docs/adr/0001-constellation-architecture.md` |
| Change the wire format | `open-prompt-edition/kit/06-interchange/wire-format.md` + `tests/fixtures/` first |
| Ship a release | `docs/build-pipeline.md`, `VERSIONING.md` |
| Edit the website | `website/` (`make serve-website`) |

## Troubleshooting

- **verify exits 75** — another verify is running; wait, retry.
- **Push blocked with "cannot execute binary file"** — bd rewrote the hook
  chain; run `./scripts/install-hooks.sh` (see docs/troubleshooting.md).
- **App shows "Could not start phantom-api"** — read
  `~/Library/Logs/Phantom/phantom-api.log`; the server's stderr lands
  there, not in the UI.

For complete troubleshooting (build failures, runtime errors, test failures, migrations, releases), see **[troubleshooting.md](troubleshooting.md)**.
