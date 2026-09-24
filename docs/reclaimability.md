# Reclaimability: the taxonomy and the measurement rules

Phantom's headline claim is not "here is what's big" — every du clone does
that — it is "here is what you would actually get back, and how to get it
back safely." This document is the contract behind that claim. The
implementation is `rust/phantom-core/src/classify.rs`; every rule below was
earned in a real cleanup incident (dates cite the disk-cleanup playbook and
docs/troubleshooting.md).

## The posture: show and suggest, never delete

Phantom has **no delete API** — not in the core, not over HTTP, not in the
CLI or MCP surface. Classification output is a category plus an
*action hint*: a human-readable suggestion naming the safe tool
(`cargo clean`, `toolbox clean`, `brew cleanup`), never an operation Phantom
performs. Two reasons, both earned:

1. Tool-managed stores corrupt when deleted out from under their owner.
   `rm -rf ~/.toolbox/tools` orphans the `<version>.json` sidecars;
   `toolbox clean` took the same store from 18 GB to 9.5 GB safely
   (2026-08-20).
2. Automated staleness heuristics have deleted live work before: a
   depth-capped mtime check misread a project edited *that morning* as
   3-weeks dormant, and five active projects lost their build trees
   (2026-08-20). A wrong *suggestion* costs a shrug; a wrong *delete* costs
   an afternoon.

## The four measurement rules

### 1. `diskSize` is THE size

`diskSize` (`st_blocks × 512`) is the headline number everywhere;
`logicalSize` is a secondary field. They diverge in both directions: sparse
files (logical ≫ disk) and cloud-dataloaded files, where OneDrive/
CloudStorage reports full logical size while occupying ~0 blocks. `du`
reads apparent size and overstated a 147 MB OneDrive tree whose physical
footprint was 0.14 GB of nothing (2026-08-20). A reclaim estimate built on
logical size promises space that was never occupied.

### 2. Cloud-dataloaded detection: the `dataless` flag first, the ratio only as a fallback

Since v1.1 the walker records `SF_DATALESS` on every entry (`flags:
["dataless"]`) — the kernel's own statement that a file's contents are not
local. That flag IS the cloud-placeholder signal. The v1.0 heuristic —
`logicalSize ≥ 8 × diskSize` AND `logicalSize ≥ 1 MiB`
(`CLOUD_DATALOADED_MIN_RATIO`, `CLOUD_DATALOADED_MIN_LOGICAL`) — now speaks
only for rows that carry no flags at all (pre-v5 rows). The reason it had
to go second: decmpfs-compressed files (`compressed`) and sparse files
(`sparse`) also show logical ≫ disk, and their bytes are local and
reclaimable; the e2e fixture's compressed `cloud.dat` read as a placeholder
candidate under the ratio alone.

The check is a per-file **override**: a placeholder inside `node_modules`
still frees ~nothing, so it moves to the cloud group instead of inflating
the regenerable estimate.

### 3. Hardlinks and clones: a sharing group counts once, and frees only when it is all inside

Entries with `nlink > 1` share blocks; deleting one path frees nothing
until the last link goes. "17 GB" of `~/.cache/uv` freed only 5 GB
(2026-08-20). APFS pure clones (`cp -c`, Finder Duplicate) are the same lie
without an `nlink` to reveal it: every clone reports the full allocation to
`st_blocks`. Since v1.1 (ADR-0006) both are one mechanism:

- **Listings** may show every path (`listedDiskSize`).
- **`diskSize`** dedupes by `ChargeKey` — `(dev, ino)` for a hardlinked
  inode, `(dev, cloneId)` for a pure-clone stream — per group for group
  totals (the du model: one allocation per group).
- **`privateSize`** (and the scan-level `reclaimEstimate`, which is its sum
  over the reclaimable categories) is what deletion would ACTUALLY free: a
  sharing group counts only if EVERY reference to it lies inside the set
  being deleted — and inside the scan at all (`ShareLedger` knows `nlink`
  and `CLONE_REFCNT`). A venv's link to the uv store contributes 0 to the
  venv's group; a Finder-duplicated project's `node_modules` contributes
  only its non-cloned files. Ungrouped files contribute the kernel's
  `PRIVATESIZE`: 0 for a snapshot-held file, the rewritten blocks for a
  modified clone, the allocation for an ordinary or a decmpfs-compressed
  file.
- `dev` is part of every key: the same inode number on two devices is two
  files.
- **Memory breaker (phantom-d1h).** The dedup set is bounded at 10 million
  distinct groups (~400 MB; the worst real tree seen was 215k in a Rust
  `target/debug`). Past the cap an untracked group is charged at every
  reference — the `du` over-count, bounded — and `LinkCharger::saturated()`
  says so; groups tracked before the cap keep deduplicating. A bounded
  over-count with a flag beats an OOM-killed scan.

### 4. Staleness = now − max(git activity, newest source mtime); missing evidence is `unverifiable`

A project root is a directory containing `.git` or any registry row's
detection file (`Cargo.toml`, `package.json`, `build.gradle`, `*.py`, …) —
except when that marker itself sits under a hotspot root (every package
inside `node_modules` ships a `package.json`). Two signals, the LATER wins
(the dev-prune rule: uncommitted work is activity, and so is a commit to
untouched sources):

- **Git activity** — the newest mtime of `<root>/.git/logs/HEAD`,
  `COMMIT_EDITMSG`, `ORIG_HEAD`, `HEAD`. The reflog moves exactly when HEAD
  does, so a commit's mtime IS the commit time, read without opening the
  repository (the classifier never touches the disk). `FETCH_HEAD` and
  `index` are deliberately ignored: an IDE's background fetch and a `git
  status` rewrite them with nobody home.
- **Newest source mtime at full depth** under the root, excluding every
  hotspot root (artifact mtimes lie: `cargo sweep` touches `target/` on
  every run) and `.git`. Depth caps lie too: a `-maxdepth 3` walk misread a
  project edited that morning as dormant (2026-08-20).

A project is **dormant** when that age is `≥ 90 days` (`DORMANT_AFTER_DAYS`;
the boundary is inclusive and pinned by test) — or ≥ the threshold the scan
asked for (`phantom scan --older 3M`, API/MCP `olderThan`: `90d`, `12w`,
`3M`, `1y`, or bare days; malformed values are a 400 at request time).
A root with NEITHER signal — no `.git` activity file in the scan and no
dated source — is **unverifiable**: never dormant, however old it looks.
Dormant + regenerable is the best reclaim candidate there is — it sorts to
the top of the list. Staleness RANKS; it never changes the tier.

## The taxonomy

Eight categories, stored in `entries.category` as the camelCase wire
string. Each carries an action hint; registry rows sharpen it.

| Category | Meaning | Posture |
|---|---|---|
| `staleProjectArtifact` | Regenerable artifact inside a dormant project | Top of the reclaim list |
| `regenerableArtifact` | A build regenerates it: an artifact directory BESIDE its detection file (the table below) | Safe to reclaim WITH its lockfile; suggest the build tool |
| `toolManagedCache` | A cache OWNED by a tool (`~/.toolbox`, `~/.cargo`, Homebrew Cellar, `~/.cache/uv`, the Docker Desktop VM disk) | Suggest the tool's own clean command — e.g. "use `toolbox clean`, not rm -rf" |
| `cache` | App/OS cache (`Library/Caches`, DerivedData, Electron caches) | Safe to reclaim; the owner rebuilds it |
| `modelCache` | Downloaded AI model weights (Ollama, Hugging Face hub, Whisper, PyTorch hub, vLLM, Triton) — v1.1 | Reclaimable, but gigabytes to re-download: caution, re-download hint, never rm -rf |
| `cloudDataloaded` | Placeholder; contents live in the cloud | Deleting frees ~nothing; excluded from the estimate |
| `reviewFirst` | Big and unclassified, or possibly holding state (git packs > 200 MB, Group Containers, agent worktrees, plain files ≥ 1 GiB) | Shown, never suggested |
| `wontRegenerate` | Deleting loses data (CloudStorage / iCloud Drive **originals** — a local delete propagates to the cloud) | Never reclaimable |

Only `staleProjectArtifact`, `regenerableArtifact`, `cache`,
`toolManagedCache` and `modelCache` count toward `reclaimEstimate`.

## The tier rubric (v1.1)

Every hotspot group STATES how risky it is instead of implying it through
the category — `riskTier`, with a one-sentence `why` and a `rebuildCost`
(cleardisk, Reclaimr and cache-commander all ship a tier; Phantom's is
derived, never asserted by a rule). The rubric is a pure function of the
registry row, the effective category and the lock state (`tier_for`):

| Tier | When | What the `why` says |
|---|---|---|
| `safe` | a regenerable artifact whose row has a lockfile concept AND a lockfile sits beside the detection file (optionally verified, see below); a regenerable row with no lockfile concept (compile output: Gradle, Maven, CMake, `__pycache__`, …); an app cache | "…; a lockfile pins the dependency versions" / nothing to add |
| `caution` | a regenerable artifact whose lockfile is MISSING (or whose parent is outside the scan — unobservable ≠ present), or whose opt-in verification FAILED; every tool-managed cache (go through the tool); every model cache (re-download is gigabytes) | "…; no lockfile (Cargo.lock) beside it, so a reinstall may resolve different versions" / "…; a lockfile is present but verification failed (`cargo metadata …: exit 101`), so …" |
| `review` | `reviewFirst`, `wontRegenerate`, `cloudDataloaded` — and every group persisted before v1.1 (decode default: unrated is not safe) | the row's clause alone |

Groups split by tier: one `cargo-target` group for the projects with a
`Cargo.lock`, another for those without. Staleness appends "; the
project's newest source edit and git activity are at least N days old" and
ranks the group first, but does not move the tier.

**Rebuild cost.** `kind` is `download` (packages, model weights, vendored
modules), `compile` (build output, bytecode, import caches) or `none`
(nothing to rebuild — a cache the owner repopulates — or nothing can bring
it back: the tier says which). The estimate is a human line sized from the
group's deduped bytes: `re-download ≈ 2.1 GB`, `re-compile ≈ 17.2 GB of
build output`.

**Opt-in lockfile verification** (`--verify-locks` / `verifyLocks`): for a
root whose present lockfile is the one its row can check, the read-only
command runs in the project directory — `cargo metadata --locked --offline
--no-deps`, `npm ci --dry-run --ignore-scripts --offline`, `uv lock
--locked --offline` — from a fixed absolute path, with a scrubbed
environment and a 20 s bound, at most 5 runs per rule per scan. Exit 0
→ `Verified` (the why names the command); anything else → `Failed` and the
tier drops to `caution`; tool absent → unverified, the lockfile still
counts. It writes nothing, and it never RAISES a tier. Read
`docs/threat-model.md` §4 before enabling it on checkouts you do not trust.

**Opt-in tool-native numbers** (`--tool-estimates` / `toolEstimates`):
`docker system df --format json` (Docker Desktop VM disk), `brew cleanup
-n` (both Cellar rows), `uv cache size` (`~/.cache/uv`) attach the tool's
OWN figure as `toolEstimate {tool, command, reclaimableBytes, note}`. Same
fixed-path / scrubbed-env rules, each under its own bound (20 s; brew
180 s — it evaluates every formula); a missing tool or a failed run leaves
it null. (`uv cache prune` has no dry-run mode in uv 0.12, so the
uv number is the whole cache — what `uv cache clean` would free.)

## The hotspot registry

Hotspot knowledge is a **data table** (`REGISTRY` in `classify.rs`), one
row per hotspot: matcher → category → hint → tier inputs. Adding a hotspot
is adding a row, not writing a function. Three matching invariants:

- **Component boundaries.** All path matching is path-component aware:
  `node_modules_backup` never matches the `node_modules` rule, and a
  dormant `/proj` never marks `/proj-two`'s artifacts stale. Substring
  matching is a review finding.
- **An artifact counts only beside its detection file.** `target/` is
  regenerable only WITH a `Cargo.toml` (or `pom.xml`, or `build.sbt`)
  beside it; a generic `build/` only with a `package.json`, `build.gradle`,
  `CMakeLists.txt` or `pubspec.yaml` (this repo's own `build/Phantom.app`
  must never classify as regenerable); `vendor/` only beside a
  `composer.json` or `go.mod`. If the detection file is outside the scan,
  the claim is unprovable and the rule stays silent — scanning an artifact
  directory DIRECTLY (root = `…/target`) classifies it as nothing, while
  the four unambiguous sibling-free names (`node_modules`, `.venv`,
  `.next`, `DerivedData`) still classify (their tier is `caution` there:
  the lockfile is unobservable). Pinned by test, and by
  `tests/fixtures/projects/` — one project per row plus `decoys/` with the
  same artifact names and no detection file, walked by the real scanner in
  the Rust suite and the e2e harness (gate G2: the exact set, zero decoys).
- **Carve-outs.** Nested hotspots collapse to the outermost root so nothing
  is counted twice — except rows marked carve-out (the model caches and
  `~/.cache/uv`), which stay their own group inside `~/.cache`; the
  enclosing group's totals exclude them, so every byte is still counted
  exactly once.

The first matching row wins, and the plain-large-file catch-all is pinned
(by test) as the registry's last row. The table (kondo's 24 project types
plus Bazel and Go vendor, the caches, the model caches, and the review
rows); `why` is the first clause, the classifier appends the rest:

| ruleId | label | matcher | category | hint (exact) | command | why (first clause, exact) | rebuild | lockfiles → `safe` | verify (opt-in) | carve-out |
|---|---|---|---|---|---|---|---|---|---|---|
| `cloud-dataloaded` | `Cloud-dataloaded placeholders` | CloudDataloadedFile | cloudDataloaded | `` contents live in the cloud; local blocks are ~0 — deleting frees almost nothing `` | null | `` a cloud placeholder whose contents are not local; deleting it frees almost no blocks `` | none | — | — |  |
| `cargo-target` | `Rust target/ directories` | ProjectArtifact(detection: `Cargo.toml`; artifacts: `target`) | regenerableArtifact | `` `cargo clean` or delete; the next `cargo build` regenerates it `` | `cargo clean` | `` Cargo build output beside a Cargo.toml; `cargo build` recreates it `` | compile | Cargo.lock | `cargo metadata --locked --offline --no-deps` |  |
| `node-modules` | `node_modules directories` | DirNamed(`node_modules`) | regenerableArtifact | `` `npm install` / `pnpm install` regenerates it `` | null | `` installed JavaScript dependencies; the package manager reinstalls them `` | download | package-lock.json / pnpm-lock.yaml / yarn.lock / bun.lock / bun.lockb | `npm ci --dry-run --ignore-scripts --offline` |  |
| `python-venv` | `Python virtualenvs` | DirNamed(`.venv`) | regenerableArtifact | `` recreate with `uv venv` / `python -m venv` and reinstall `` | null | `` a Python virtualenv; recreating it reinstalls the packages `` | download | uv.lock / poetry.lock / Pipfile.lock / pdm.lock | `uv lock --locked --offline` |  |
| `swiftpm-build` | `SwiftPM .build directories` | ProjectArtifact(detection: `Package.swift`; artifacts: `.build`, `.swiftpm`) | regenerableArtifact | `` `swift build` regenerates it `` | `swift package clean` | `` SwiftPM build output beside a Package.swift; `swift build` recreates it `` | compile | Package.resolved | — |  |
| `next-build` | `.next build output` | DirNamed(`.next`) | regenerableArtifact | `` `next build` regenerates it `` | null | `` Next.js build output; `next build` recreates it `` | compile | — | — |  |
| `js-build` | `JS build/ output` | ProjectArtifact(detection: `package.json`; artifacts: `build`) | regenerableArtifact | `` the package's build script regenerates it `` | null | `` build output beside a package.json; the package's build script recreates it `` | compile | — | — |  |
| `js-dist` | `JS dist/ output` | ProjectArtifact(detection: `package.json`; artifacts: `dist`) | regenerableArtifact | `` the package's build script regenerates it `` | null | `` bundled output beside a package.json; the package's build script recreates it `` | compile | — | — |  |
| `react-native-cache` | `React Native / Expo caches` | ProjectArtifact(detection: `package.json`; artifacts: `.expo`, `.metro`) | regenerableArtifact | `` Expo / Metro bundler caches; the next `expo start` rebuilds them `` | null | `` Expo and Metro bundler caches beside a package.json; the next start rebuilds them `` | compile | — | — |  |
| `turborepo-cache` | `Turborepo .turbo caches` | ProjectArtifact(detection: `turbo.json`; artifacts: `.turbo`) | regenerableArtifact | `` Turborepo task cache; the next `turbo run` rebuilds it `` | null | `` Turborepo task cache beside a turbo.json; the next run rebuilds it `` | compile | — | — |  |
| `gradle-build` | `Gradle build output` | ProjectArtifact(detection: `build.gradle`, `build.gradle.kts`, `settings.gradle`, `settings.gradle.kts`; artifacts: `build`, `.gradle`) | regenerableArtifact | `` `gradle clean` or delete; the next build regenerates it `` | `gradle clean` | `` Gradle build output beside a build.gradle; the next build recreates it `` | compile | — | — |  |
| `maven-target` | `Maven target/ directories` | ProjectArtifact(detection: `pom.xml`; artifacts: `target`) | regenerableArtifact | `` `mvn clean` or delete; the next `mvn package` regenerates it `` | `mvn clean` | `` Maven build output beside a pom.xml; `mvn package` recreates it `` | compile | — | — |  |
| `sbt-target` | `sbt target/ directories` | ProjectArtifact(detection: `build.sbt`; artifacts: `target`) | regenerableArtifact | `` `sbt clean` or delete; the next `sbt compile` regenerates it `` | `sbt clean` | `` sbt build output beside a build.sbt; `sbt compile` recreates it `` | compile | — | — |  |
| `cmake-build` | `CMake build directories` | ProjectArtifact(detection: `CMakeLists.txt`; artifacts: `build`, `cmake-build-*`) | regenerableArtifact | `` delete and re-run `cmake -B build`; the next build regenerates it `` | null | `` CMake build tree beside a CMakeLists.txt; configuring again recreates it `` | compile | — | — |  |
| `unity-library` | `Unity Library / Temp / Obj / Logs` | ProjectArtifact(detection: `Assembly-CSharp.csproj`, `ProjectSettings`; artifacts: `Library`, `Temp`, `Obj`, `Logs`) | regenerableArtifact | `` Unity reimports the Library on next open (slow for big projects) `` | null | `` Unity's import cache and temp output beside the project settings; reopening the project reimports them `` | compile | — | — |  |
| `unreal-intermediate` | `Unreal Binaries / Intermediate / DerivedDataCache` | ProjectArtifact(detection: `*.uproject`; artifacts: `Binaries`, `Intermediate`, `DerivedDataCache`) | regenerableArtifact | `` Unreal regenerates them on the next build / editor open `` | null | `` Unreal build intermediates beside a .uproject; the next build recreates them `` | compile | — | — |  |
| `python-caches` | `Python tool caches (.mypy_cache, .pytest_cache, .tox, …)` | ProjectArtifact(detection: pyproject.toml, setup.py, setup.cfg, requirements.txt, Pipfile, tox.ini; artifacts: `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.tox`, `.nox`, `__pypackages__`) | regenerableArtifact | `` the tools rebuild them on the next run `` | null | `` Python tool caches beside a project manifest; mypy, pytest, ruff, tox recreate them `` | compile | — | — |  |
| `python-bytecode` | `__pycache__ directories` | ProjectArtifact(detection: `*.py`; artifacts: `__pycache__`) | regenerableArtifact | `` Python rewrites bytecode on the next import `` | null | `` compiled bytecode beside its .py sources; the next import rewrites it `` | compile | — | — |  |
| `jupyter-checkpoints` | `Jupyter .ipynb_checkpoints` | ProjectArtifact(detection: `*.ipynb`; artifacts: `.ipynb_checkpoints`) | regenerableArtifact | `` autosave copies of the notebooks beside them `` | null | `` notebook autosave checkpoints beside their .ipynb files; Jupyter recreates them on save `` | none | — | — |  |
| `pixi-env` | `Pixi .pixi environments` | ProjectArtifact(detection: `pixi.toml`; artifacts: `.pixi`) | regenerableArtifact | `` `pixi install` recreates it `` | null | `` Pixi environments beside a pixi.toml; `pixi install` recreates them `` | download | pixi.lock | — |  |
| `flutter-build` | `Flutter/Dart .dart_tool and build` | ProjectArtifact(detection: `pubspec.yaml`; artifacts: `.dart_tool`, `build`) | regenerableArtifact | `` `flutter clean`; the next `flutter pub get` / build regenerates them `` | `flutter clean` | `` Dart/Flutter build output beside a pubspec.yaml; `flutter build` recreates it `` | compile | pubspec.lock | — |  |
| `elixir-build` | `Elixir _build directories` | ProjectArtifact(detection: `mix.exs`; artifacts: `_build`, `.elixir_ls`) | regenerableArtifact | `` `mix clean`; the next `mix compile` regenerates it `` | `mix clean` | `` Mix build output beside a mix.exs; `mix compile` recreates it `` | compile | mix.lock | — |  |
| `zig-cache` | `Zig cache and output` | ProjectArtifact(detection: `build.zig`; artifacts: `zig-cache`, `.zig-cache`, `zig-out`) | regenerableArtifact | `` the next `zig build` regenerates them `` | null | `` Zig build cache and output beside a build.zig; `zig build` recreates them `` | compile | — | — |  |
| `godot-import` | `Godot .godot import cache` | ProjectArtifact(detection: `project.godot`; artifacts: `.godot`) | regenerableArtifact | `` Godot reimports assets on next open `` | null | `` Godot's import cache beside a project.godot; reopening the project reimports it `` | compile | — | — |  |
| `dotnet-bin-obj` | `.NET bin/ and obj/` | ProjectArtifact(detection: `*.csproj`, `*.fsproj`, `*.sln`; artifacts: `bin`, `obj`) | regenerableArtifact | `` `dotnet clean`; the next `dotnet build` regenerates them `` | `dotnet clean` | `` .NET build output beside a project file; `dotnet build` recreates it `` | compile | — | — |  |
| `terraform-providers` | `Terraform .terraform directories` | ProjectArtifact(detection: `*.tf`, `.terraform.lock.hcl`; artifacts: `.terraform`) | regenerableArtifact | `` `terraform init` re-downloads providers and modules `` | null | `` downloaded Terraform providers and modules beside the configuration; `terraform init` refetches them `` | download | .terraform.lock.hcl | — |  |
| `cocoapods-pods` | `CocoaPods Pods/ directories` | ProjectArtifact(detection: `Podfile`; artifacts: `Pods`) | regenerableArtifact | `` `pod install` regenerates it `` | null | `` installed CocoaPods beside a Podfile; `pod install` reinstalls them `` | download | Podfile.lock | — |  |
| `composer-vendor` | `Composer vendor/ directories` | ProjectArtifact(detection: `composer.json`; artifacts: `vendor`) | regenerableArtifact | `` `composer install` regenerates it `` | null | `` installed PHP dependencies beside a composer.json; `composer install` reinstalls them `` | download | composer.lock | — |  |
| `go-vendor` | `Go vendor/ directories` | ProjectArtifact(detection: `go.mod`; artifacts: `vendor`) | regenerableArtifact | `` `go mod vendor` regenerates it `` | null | `` vendored Go modules beside a go.mod; `go mod vendor` refetches them `` | download | go.sum | — |  |
| `stack-work` | `Haskell .stack-work directories` | ProjectArtifact(detection: `stack.yaml`; artifacts: `.stack-work`) | regenerableArtifact | `` `stack clean`; the next `stack build` regenerates it `` | `stack clean` | `` Stack build output beside a stack.yaml; `stack build` recreates it `` | compile | — | — |  |
| `cabal-dist` | `Haskell dist-newstyle directories` | ProjectArtifact(detection: `cabal.project`, `*.cabal`; artifacts: `dist-newstyle`) | regenerableArtifact | `` `cabal clean`; the next `cabal build` regenerates it `` | `cabal clean` | `` Cabal build output beside a cabal project; `cabal build` recreates it `` | compile | — | — |  |
| `bazel-output` | `Bazel bazel-* output links` | ProjectArtifact(detection: `WORKSPACE`, `WORKSPACE.bazel`, `WORKSPACE.bzlmod`, `MODULE.bazel`; artifacts: `bazel-*`) | regenerableArtifact | `` `bazel clean`; the output base itself lives under the Bazel cache `` | `bazel clean` | `` Bazel output beside a WORKSPACE or MODULE.bazel; `bazel build` recreates it `` | compile | — | — |  |
| `xcode-derived-data` | `Xcode DerivedData` | DirNamed(`DerivedData`) | cache | `` Xcode regenerates it on the next build `` | null | `` Xcode's per-project build cache; the next build recreates it `` | compile | — | — |  |
| `library-caches` | `Library/Caches` | ChildOfDirSuffix(`Library`, `Caches`) | cache | `` per-app caches; apps rebuild them on demand (Library/Caches itself cannot be moved) `` | null | `` a per-app cache folder under Library/Caches; the app repopulates it on demand `` | none | — | — |  |
| `electron-app-cache` | `Electron app caches` | DirNamedWithin(`Cache`, `Code Cache`, `GPUCache`, `DawnCache`, `CachedData`, `Service Worker`; within `Application Support`) | cache | `` Electron/Chromium cache; the app rebuilds it `` | null | `` a Chromium-style cache inside an Electron app's support directory; the app rebuilds it `` | none | — | — |  |
| `group-containers` | `Library/Group Containers` | DirSuffix(`Library`, `Group Containers`) | reviewFirst | `` shared app-group data; apps can lose state — review per container `` | null | `` shared app-group data that may hold state no app can rebuild `` | none | — | — |  |
| `ollama-models` | `Ollama models` | DirSuffix(`.ollama`, `models`) | modelCache | `` `ollama list` then `ollama rm <model>`; `ollama pull` re-downloads `` | null | `` downloaded Ollama model weights; `ollama pull` fetches them again `` | download | — | — | yes |
| `huggingface-hub` | `Hugging Face hub cache` | DirSuffix(`huggingface`, `hub`) | modelCache | `` `huggingface-cli delete-cache` picks revisions; models re-download on next load `` | null | `` downloaded Hugging Face models and datasets; the library re-downloads them on next load `` | download | — | — | yes |
| `whisper-models` | `Whisper models` | DirSuffix(`.cache`, `whisper`) | modelCache | `` Whisper re-downloads a model the next time it is loaded `` | null | `` downloaded Whisper model weights; the next load re-downloads them `` | download | — | — | yes |
| `torch-hub` | `PyTorch hub cache` | DirSuffix(`.cache`, `torch`, `hub`) | modelCache | `` `torch.hub` re-downloads checkpoints on next use `` | null | `` downloaded PyTorch hub checkpoints; torch.hub re-downloads them on next use `` | download | — | — | yes |
| `vllm-cache` | `vLLM cache` | DirSuffix(`.cache`, `vllm`) | modelCache | `` vLLM rebuilds its cache on the next server start `` | null | `` vLLM's compiled kernels and model cache; the next server start rebuilds it `` | download | — | — | yes |
| `triton-cache` | `Triton kernel cache` | DirSuffix(`.triton`, `cache`) | modelCache | `` Triton recompiles kernels on demand `` | null | `` Triton's compiled GPU kernels; they recompile on demand `` | compile | — | — | yes |
| `uv-cache` | `uv cache` | DirSuffix(`.cache`, `uv`) | toolManagedCache | `` `uv cache prune` drops unused entries; `uv cache clean` empties it — venvs hardlink into it, so it frees less than it lists `` | `uv cache prune` | `` uv's package cache; venvs hardlink into it and uv refetches what they need `` | download | — | — | yes |
| `dot-cache` | `~/.cache` | DirNamed(`.cache`) | toolManagedCache | `` per-tool clean commands (`uv cache clean`, `pnpm store prune`); hardlinked stores free less than they list `` | null | `` the shared per-tool cache directory; each tool refetches its own entries `` | download | — | — |  |
| `dot-npm` | `~/.npm` | DirNamed(`.npm`) | toolManagedCache | `` `npm cache clean --force` `` | `npm cache clean --force` | `` npm's package cache; npm refetches packages on the next install `` | download | — | — |  |
| `dot-cargo` | `~/.cargo` | DirNamed(`.cargo`) | toolManagedCache | `` cargo registry/git caches; prune with cargo tooling, not rm -rf `` | null | `` cargo's registry and git caches (and installed binaries); cargo refetches crates on the next build `` | download | — | — |  |
| `dot-rustup` | `~/.rustup` | DirNamed(`.rustup`) | toolManagedCache | `` `rustup toolchain uninstall` unused toolchains `` | null | `` installed Rust toolchains; rustup re-downloads one on demand `` | download | — | — |  |
| `dot-toolbox` | `~/.toolbox` | DirNamed(`.toolbox`) | toolManagedCache | `` use `toolbox clean`, not rm -rf `` | `toolbox clean` | `` toolbox-managed tool versions with sidecar metadata; only `toolbox clean` removes them consistently `` | download | — | — |  |
| `homebrew-cellar` | `Homebrew Cellar` | DirSuffix(`homebrew`, `Cellar`) | toolManagedCache | `` `brew cleanup` / `brew uninstall`, not rm -rf `` | `brew cleanup` | `` installed Homebrew formulae; `brew cleanup` drops superseded versions and `brew install` refetches `` | download | — | — |  |
| `homebrew-cellar-intel` | `Homebrew Cellar (Intel prefix)` | DirSuffix(`local`, `Cellar`) | toolManagedCache | `` `brew cleanup` / `brew uninstall`, not rm -rf `` | `brew cleanup` | `` installed Homebrew formulae; `brew cleanup` drops superseded versions and `brew install` refetches `` | download | — | — |  |
| `docker-desktop-data` | `Docker Desktop VM disk` | DirSuffix(`com.docker.docker`, `Data`) | toolManagedCache | `` `docker system df` shows what is reclaimable inside; `docker system prune` frees it `` | `docker system prune` | `` the Docker Desktop VM disk image; images and volumes inside it are freed by `docker system prune`, never by deleting the file `` | download | — | — |  |
| `cloud-synced-originals` | `Cloud-synced originals (CloudStorage)` | DirSuffix(`Library`, `CloudStorage`) | wontRegenerate | `` synced originals; a local delete propagates to the cloud copy `` | null | `` cloud-synced originals; a local delete propagates to the cloud copy `` | none | — | — |  |
| `icloud-drive` | `Cloud-synced originals (iCloud Drive)` | DirSuffix(`Library`, `Mobile Documents`) | wontRegenerate | `` synced originals; a local delete propagates to the cloud copy `` | null | `` iCloud Drive originals; a local delete propagates to the cloud copy `` | none | — | — |  |
| `agent-sessions` | `Agent session data (~/.claude/projects)` | DirSuffix(`.claude`, `projects`) | reviewFirst | `` agent session history; prune old sessions after review `` | null | `` agent session transcripts that nothing regenerates `` | none | — | — |  |
| `agent-worktrees` | `Agent worktrees` | DirNamed(`.worktrees`) | reviewFirst | `` worktrees can hold uncommitted work; check `git status` in each `` | null | `` git worktrees that may hold uncommitted work `` | none | — | — |  |
| `git-pack` | `Large git pack files` | GitPackFile(> 200 MiB) | reviewFirst | `` repository history; `git gc` / repack or re-clone shallow — review first `` | null | `` repository history; only a repack or a shallow re-clone shrinks it `` | none | — | — |  |
| `large-file` | `Large files` | LargeFile(≥ 1 GiB) — LAST row | reviewFirst | `` big and unclassified; review before deleting `` | null | `` a large file no rule recognizes `` | none | — | — |  |

## Output

`classify(entries, now)` (or `classify_with(entries, &shares, now)` with the
walker's `ShareLedger`, or `classify_with_options` with the threshold and
the injected lock verifier) is a pure post-pass: per-entry category
assignments plus a `HotspotsSummary` — groups (rule, category, hint,
`riskTier`, `why`, `rebuildCost`, nullable `toolEstimate`, deduped
`diskSize`, naive `listedDiskSize`, deletion-honest `privateSize`, and
`topPaths` — every root worth acting on: private bytes ≥ 1 GiB, never fewer
than five roots, never more than 25; the plan acts on exactly this list, so
since 1.1.1 seven 9 GB worktree targets in one group are seven plan paths,
not five and a rescan) and the
scan-level rollups (`reclaimEstimate` = Σ `privateSize`, `reviewDiskSize`,
and the dataloaded logical-vs-disk pair that quantifies the du-lie). The wire
shape is camelCase at every depth, pinned by
`tests/fixtures/hotspots-summary.json`.
