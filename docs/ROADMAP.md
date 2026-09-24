# Roadmap

The durable, directional view. Beads (`bd ready`) tracks the work; this file
tracks the *shape* — where the project is going and what is explicitly out of
scope. Update it when direction changes, not when tasks close.

**Where we are (2026-09):** v1.0.0 shipped — physical sizes everywhere,
hardlink dedup, cloud-placeholder detection, the Reclaimable classifier,
persisted scan history with diff, and one local API behind an app, a CLI and
an MCP server. Auto-update via Sparkle.

## Now (v1.1 — measurement fidelity first)

The headline number has one known lie left, and the fix also makes scans
faster:

- **APFS clone awareness.** `st_blocks` double-counts cloned files (Finder
  "Duplicate", `cp -c`, some checkouts), so a cloned tree reads as
  reclaimable when deleting it frees almost nothing. Move the walker to
  `getattrlistbulk` and report reclaimable bytes from each file's *private*
  size, counting a clone group once — the same discipline already applied to
  hardlinks and dataless files.
- **Shared vs unique bytes** per directory, so "delete this venv" shows what
  a hardlinked or cloned store would actually return.
- **Classifier breadth.** Two dozen more project types (Gradle, Maven, CMake,
  Python tool caches, Flutter, Zig, .NET, Terraform, CocoaPods, Unity, …),
  and a new category for **AI model caches** (Ollama, HuggingFace, Whisper,
  PyTorch) — the newest silent growth on developer Macs. Every reclaimable
  item gains an explicit risk tier, a one-line *why*, and its rebuild cost.
- **Staleness that protects uncommitted work:** later of last commit and
  newest source mtime; "unverifiable" is its own state.
- **Agent surface hardening:** MCP tool annotations and structured output;
  `plan_reclaim` (ordered dry-run plan with exact commands and expected
  bytes) and `verify_reclaim` (before/after via scan diff); async scan
  status, cancel and progress; a shipped skill describing the
  scan → plan → confirm → run → verify workflow. Phantom still never deletes;
  execution stays in the user's or the agent's shell.

## Next (v1.2 — "Always current"; plan in the working repo)

History that keeps itself. Chosen 2026-09-09 over a clone-aware duplicate
finder (v1.3) and a table-stakes release.

- **Schedules**, owned by the API and run while the app is open: daily or
  weekly per root, exclusions attached, one scan per window even after a
  missed one. Nothing runs when Phantom is not running; a launchd agent is
  a later, opt-in step with its own threat-model section.
- **Growth notification** after a scheduled scan — "~ grew 20 GB this week,
  14 GB of it DerivedData" — with high default thresholds, silence on
  shrinkage, and one notification per scan ever.
- **Exclusions parity** (gitignore-syntax ignore files, `CACHEDIR.TAG`,
  per-root rules) and the scan-options sheet in the app.
- **Incremental rescans via FSEvents — only if measured.** Fourteen daily
  scans decide; under five minutes a full scan is fine.
- If there is slack: ncdu-JSON export, file-age views and a filter syntax,
  progressive results, the measured perf backlog.

## Later / someday

- **Duplicate finder, clone-aware.** Group by size, prehash, then full hash;
  files that already share blocks report 0 B reclaimable. The reclaim action
  is `cp -c` — converting true duplicates into clones frees space without
  deleting a byte, which nothing else on macOS offers.
- **Scans while the app is closed** (a launchd agent), once scheduled scans
  inside the app have shown their cost.
- Streamable-HTTP MCP transport on the existing loopback API; scans as MCP
  resources; protocol currency with the 2026 spec revisions.
- Lints: empty directories, broken symlinks, orphaned installers,
  multi-hardlink report.

## Explicitly not doing

Record rejected directions WITH the reason — future-you will re-derive the
idea and needs to know why it lost last time.

- **Deleting files, even to Trash.** Every 2026 entrant defaults to
  Trash-with-undo, and it is the most-requested feature in the segment. It
  stays out because the value of a *measurement* tool is that its numbers
  cannot be blamed for a loss, and because the review history of tools that
  do delete (broken cloud sync, "uninstalled" apps still present) is the
  argument. Phantom emits the exact command, and in v1.1 an executable
  dry-run plan; the user or their agent runs it where the OS and the agent
  host already ask for confirmation.
- **Cloud-quota scanning via provider OAuth.** Conflicts with zero egress.
  Phantom measures what is on this disk; a dataless placeholder is 0 bytes.
- **APFS purgeable-space *math*.** Apple's own tools disagree with each
  other. Phantom will *report* what the OS exposes and the container
  arithmetic (see "Where is System Data?"), not model it.
- **System-pressure monitoring.** Belongs to a different tool (ADR-0004).
- **A treemap-first pitch.** Visualization is a solved, crowded space;
  Phantom's treemap is a view, not the product.
