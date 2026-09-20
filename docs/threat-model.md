# Threat model

Four standing questions, answered for the product Phantom actually is: a
disk-usage scanner that reads the whole filesystem, classifies what it finds,
and — since v1.1 Phase 2 — can, on explicit opt-in, run a handful of
third-party tools in read-only modes. Re-answer them when the domain grows a
new surface (network sync, sharing, cloud backup, any write to the
filesystem — each invalidates parts of this analysis).

**This file provides the analysis; [../SECURITY.md](../SECURITY.md) states the
public-facing claims.** When you update this threat model, update SECURITY.md
in the same commit.

## 1. What are we protecting?

1. **The user's files.** Phantom reads everything Full Disk Access lets it
   read. It must never delete, move, truncate or rewrite any of it. There is
   no delete API on any surface; the only "actions" are a copied command and
   Reveal in Finder (`docs/reclaimability.md`).
2. **The scan database**
   (`~/Library/Application Support/phantom/phantom.db`): a map of the
   user's filesystem — every path ≥ 1 MiB, every directory, sizes, mtimes,
   categories. Confidentiality (it names files), integrity (a corrupted
   summary suggests the wrong cleanup), availability (history is the
   `diff_scans` feature).
3. **The user's judgment.** The product's output is advice. A wrong
   *suggestion* costs a shrug; a wrong suggestion stated as `safe` with a
   number behind it costs an afternoon. Classifier honesty is a security
   property here: `riskTier`, `why` and `privateSize` must not overstate.

## 2. From whom?

| Adversary | In scope? | Mechanism |
|---|---|---|
| Other local users on the Mac | **yes** | key-file auth (0600) + loopback-only bind |
| Web pages doing localhost port scans / DNS rebinding | **yes** | requests without `X-Api-Key` get 401; the key never leaves local files |
| Network attackers | no | server binds 127.0.0.1 only; no listening surface; the API, CLI and MCP never dial out |
| Processes running as the same user | no | same-user malware can read the key file and the DB; that battle is the OS's |
| **A hostile checkout on disk** (a cloned repo whose config files are attacker-controlled) | **yes, since v1.1** | see §4 — the default scan never executes anything; the opt-in verify probes run tools INSIDE such directories and are bounded as described below |
| The developer (us) shipping a bad migration | **yes** | forward-only migrations + refuse-newer-schema + verified backups (`docs/data-safety.md`) |
| An agent (MCP client) acting on Phantom's advice | **yes** | Phantom exposes no destructive tool; advice carries `riskTier`/`why`/`privateSize` so an agent cannot read "regenerable" as "free" |
| Theft of the powered-off machine | no | FileVault's job; documented in SECURITY.md |

## 3. What are the failure costs?

Data loss > data disclosure for this class of tool, and *induced* data loss
(the user deletes what we called safe) is the realistic path to it. Hence:
no delete API anywhere; `safe` requires a present lockfile; the estimate is
`privateSize`, never `diskSize`; unverifiable projects are never "stale".

## 4. Subprocesses (v1.1 Phase 2, phantom-mkn.5 / phantom-mkn.19)

Three opt-in features run executables. **None runs by default**; a scan
request must set `verifyLocks: true` and/or `toolEstimates: true` (CLI
`--verify-locks`, `--tool-estimates`; MCP `verifyLocks`, `toolEstimates`),
and a volume request must set `snapshots=true` (Phase 3). The app exposes
none of them in v1.1.

| Feature | What runs | Where | Reads | Writes |
|---|---|---|---|---|
| Lockfile verification | `cargo metadata --locked --offline --no-deps`, `npm ci --dry-run --ignore-scripts --offline`, `uv lock --locked --offline` | in the PROJECT directory holding the lockfile | that project's manifest, lockfile and local caches | nothing (each command is the tool's own no-write mode; `--offline` forbids network) |
| Tool-native estimates | `docker system df --format json`, `brew cleanup -n`, `uv cache size` | no cwd dependence | the tool's own store | nothing (dry-run / read-only subcommands) |
| Volume snapshots (Phase 3, `GET /volume?snapshots=true`; MCP `get_volume_status {snapshots: true}`; CLI `volume --snapshots`) | `/usr/bin/tmutil listlocalsnapshots <mountPoint>` | no cwd dependence | Time Machine's snapshot list | nothing (a listing subcommand). Fixed path only, 10 s bound, output kept only as snapshot NAMES matching `com.apple.TimeMachine.*.local`. Off unless the request asks |

Rules, all enforced in `rust/phantom-core/src/probe.rs` and pinned by test:

- **Fixed absolute paths only.** Each tool has a short list of well-known
  install locations (`~/.cargo/bin/cargo`, `/opt/homebrew/bin/brew`,
  `/usr/local/bin/docker`, …). The first that exists is used; `$PATH` is
  never consulted. A tool at none of them is `unavailable`, not searched
  for. Rationale: a scan may traverse directories an attacker controls; a
  `$PATH` lookup could be poisoned by a same-user process, and a relative
  lookup by the cwd.
- **Scrubbed environment.** Children get `HOME`, a `PATH` consisting of
  the tool's own directory (rustup's proxies need to find their toolchain),
  and nothing else from Phantom's environment.
- **Bounded.** Each run has a wall-clock timeout (20 s; `brew cleanup -n`
  gets 180 s because it evaluates every formula); on expiry
  the child is killed and the verdict is `failed (timed out)`. The number
  of verify runs per scan is capped (the largest roots per group), so a
  scan of a code folder with hundreds of projects cannot fan out into
  hundreds of processes.
- **Output is data, not instructions.** stdout/stderr are parsed into a
  number or a short reason; at most a short excerpt is stored, never the
  raw output, and nothing from them is executed or interpolated into a
  command.
- **Fail closed.** Any failure of a verify probe lowers the tier
  (`caution`) and says why; it never raises it. A tool-estimate failure
  leaves `toolEstimate: null`.

**Residual risk, stated plainly.** Verification runs a build tool *inside a
directory Phantom did not author*. Build tools honour project-local
configuration: Cargo reads `.cargo/config.toml` and will invoke whatever
`build.rustc` names when computing metadata; npm reads a project `.npmrc`;
uv may need to evaluate a `pyproject.toml` build backend when a lockfile is
incomplete. `--offline`, `--locked`, `--no-deps`, `--ignore-scripts` and the
scrubbed environment narrow this, they do not close it. **Do not enable
`verifyLocks` when scanning checkouts you do not trust.** The default scan,
and every tool-estimate probe, has no such exposure: nothing under the
scanned tree is ever executed or evaluated.

## 5. What changes the model?

- **Any outbound network call** (update checks, sync): update **THIS FILE**
  and **[../SECURITY.md](../SECURITY.md)** in the same commit. Today the ONE
  outbound connection is the app's Sparkle update check (SECURITY.md).
- **Any new subprocess**, or a new mode for an existing one: add it to the
  table in §4, to `probe.rs`'s fixed-path list, and to SECURITY.md's list.
  A subprocess that writes — even to its own cache — is a new model, not a
  row.
- **Any write to the scanned filesystem** (a delete, a move, a "clean"
  Phantom performs itself): this is a posture change the project has
  standing decisions against (`docs/ROADMAP.md`); it would invalidate §1.
- **Multi-device sync / sharing / exports:** transport auth, at-rest
  posture and conflict handling all enter scope — a new model, not an edit.
