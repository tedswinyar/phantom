# Positioning — one-pager

_The website copy, the README pitch, and any announcement are downstream of
this page. Update it here first._

## The one-liner

> **Phantom** is a disk-space analyzer for macOS developers whose drives fill
> up with build artifacts, tool caches, and cloud-synced files, that measures
> what every file actually occupies on disk and classifies what is safe to
> reclaim — unlike treemap tools, which show you rectangles and leave the
> judgment to you.

## Audience

- **Who exactly?** Developers on Macs — the person whose 460 GB drive hits
  89% full and who knows, roughly, that the culprits are Rust `target/`
  directories, `node_modules`, Xcode DerivedData, tool-manager caches, and a
  OneDrive folder of unknown physical weight. Increasingly: the person whose
  AI agents spawn worktrees that each bring their own build tree.
- **What do they do today instead?** `du -sh` one-liners (which lie about
  hardlinks, cloud files, and dotfiles), a treemap app (DaisyDisk,
  GrandPerspective — pretty, but every judgment call is still theirs), or a
  panic-driven cleanup session with `rm -rf` and crossed fingers.
- **What would make them switch?** One thing: a scan that ends in a
  trustworthy answer to "what can I reclaim, and how?" — not another picture
  of the problem.

## The hook

**Reclaimable.** Phantom doesn't stop at showing where the bytes are; it
classifies them — regenerable build artifacts, tool-managed caches,
cloud-dataloaded files, stale project artifacts — estimates what a cleanup
would actually free, and suggests the safe command for each category.
It never deletes anything itself. The demo moment: scan a home directory,
open the Reclaimable view, and watch tens of gigabytes sort themselves into
"one command, no judgment required" versus "review first."

Supporting hooks (not the headline):

- **Every size is physical.** `st_blocks`, not apparent size, everywhere —
  app, CLI, and MCP agree byte-for-byte. `du` will report a cloud-dataloaded
  OneDrive tree at 147 MB when its physical footprint is approximately zero,
  and will promise you 17 GB from a hardlinked cache that frees 5 GB.
  Phantom reports what deletion would actually return.
- **Agent-native.** The CLI is scriptable (`--json`, stable exit codes) and
  the MCP server gives AI agents the same scan data with the same authority.
- **Local-only.** Loopback-only API, key-file auth, zero egress.

## Message hierarchy (positioning → website)

| Positioning output | Theme slot | Where it landed |
|---|---|---|
| The hook (one sentence) | `hero.headline` | "Your disk is full of ghosts" |
| One-liner benefit | `hero.tagline` | Scan, measure physically, classify what's reclaimable — never delete |
| Audience + JTBD | `hero.audience` | Developers with drives full of build artifacts and caches |
| The hook (expanded) | `sections` + `integration` | Reclaimable section; MCP section |
| Physical-size honesty | `highlights` cards | First highlight card |
| Proof table (below) | body copy | Woven into features page claims |
| Non-goals | `/architecture/` + this doc | Not the landing page |

## Proof

| Claim | Evidence |
|---|---|
| Apparent size lies about cloud files | 2026-08-20 session: `du` reported a 147 MB OneDrive tree whose physical footprint was ~0 (fully dataloaded). Recorded in the disk-cleanup playbook; drove the `cloudDataloaded` category (logicalSize ≫ diskSize). |
| Apparent size lies about hardlinked stores | `~/.cache/uv` measured 17 GB by `du`; deleting it freed 5 GB — the rest of the blocks were shared with live installs. Phantom dedupes by `(dev, ino)`. |
| `df -h /` lies on APFS | Reported "140Mi avail" while the data volume had 21 GiB (2026-08-20). Phantom's docs and CLI point at `/System/Volumes/Data`. |
| Artifact mtimes lie about staleness | A pruner keyed on `target/` mtimes never aged anything out (`cargo sweep` re-touches them); a depth-capped source check misread a project edited that morning as 3 weeks dormant. Phantom's staleness analyzer walks source files at full depth. |
| Orphaned caches accumulate silently | Nine orphaned Xcode DerivedData directories totalling 24 GB from one renamed project (2026-08-20 session). A registry entry, not a special case. |
| All clients agree | The e2e harness asserts CLI, raw HTTP, and MCP views of the same scan match byte-for-byte, on every push. (The v0.1 CLI and MCP used logical size while the app used physical — three tools, two answers. v1.0 makes that divergence structurally impossible.) |
| Nothing leaves the machine | API binds 127.0.0.1 only, key-file auth, no outbound calls — see `docs/threat-model.md` and `SECURITY.md`. |
| Apparent size lies about APFS clones (v1.1) | `~/Code` on the dev Mac: v1.0's totals were 3.68 GB high — clones counted once per copy. The 1.1 walker reads the clone attributes and reports diskSize / privateSize / sharedSize (ADR-0006). |
| `statfs` lies about the volume (v1.1) | `f_blocks − f_bfree` on the data volume is the whole APFS container (386.7 GB) while the volume alone used 363.5 GB (`getattrlist ATTR_VOL_SPACEUSED`, matches `diskutil apfs list`). Phantom's hidden-space split uses the per-volume figure; the gap is the other volumes. |
| An agent can close the loop (v1.1) | Gate G3: a fresh agent session with only the MCP tools and the bundled skill runs scan → plan → confirm → run → verify within 5 % of the estimate. (2026-09-04, pre-1.1: an agent over MCP found 94.9 GB via `diff_scans` during a disk emergency.) |

## Non-goals

- **Deleting files.** Phantom shows, classifies, and suggests the command;
  the user (or their agent) executes it. v1.1 adds the plan → script →
  verify loop, and the script still only MOVES TO TRASH when the user runs
  it with `PHANTOM_APPLY=1`; nothing in Phantom unlinks.
- **Cloud-file eviction actions** (triggering "Free Up Space") — suggested,
  never performed.
- **APFS purgeable-space math** — not modeled. v1.1 REPORTS purgeable
  (CoreFoundation's important-usage capacity minus statfs available) and
  decomposes the used-minus-scanned gap read-only; it never predicts what
  macOS will purge or when.
- **Scheduled scans** — post-1.1 (v1.2 with FSEvents incremental rescans).
  Scan *diffing* shipped in 1.0; 1.1 adds history, the growth forecast (with
  its caveat), `phantom diff --since`, and new-since-last-scan marks.
- ~~Auto-update (Sparkle) — post-1.0~~ — pulled INTO 1.0 (2026-09-02):
  the first shared build must be able to update itself; appcast on the
  public repo's GitHub Releases, EdDSA-signed (SECURITY.md).
- **System-pressure monitoring** — belongs to Banshee, not Phantom
  (ADR-0004).
