# 05 — Algorithms

Non-obvious logic an independent implementation must reproduce exactly.
"Non-obvious" means: two reasonable implementers would write it differently
and their outputs would diverge. References: `rust/phantom-core/src/treemap.rs`,
`rust/phantom-core/src/classify.rs`, `docs/reclaimability.md`.

## Port ladder

Given configured port P ≠ 0: attempt to bind `P, P+1, …, P+9` in order,
first success wins, announce it on stdout, else exit nonzero. P = 0 binds an
ephemeral port directly (no ladder). (Spec: `../04-config/config-spec.md`.)

## Persistence aggregation (ADR-0005)

At scan completion, before the entry filter drops sub-1-MiB file rows,
every directory's `diskSize`/`logicalSize` is replaced by the sum over ALL
descendant files at full depth (walking each file's ancestor chain up to the
scan root; the first ancestor outside the scan stops the walk). Consequence:
a persisted directory row's aggregate can exceed the sum of its persisted
children — the difference is the filtered small-file remainder. The treemap
below leans on this.

Pinned by: conformance ("root aggregate == scan total") and
`tests/e2e/run-e2e.sh` (sub/ aggregate includes its filtered file).

## Deterministic orderings

Ties are where divergence hides; every listing has a total order:

| Surface | Order |
|---|---|
| `GET /scans` | `startedAt` descending, then id ascending |
| `/files` sort=size | `diskSize` descending, then `path` ascending |
| `/files` sort=name | `name` ascending, then `path` |
| `/files` sort=path, `/tree` | `path` ascending (unique per scan — total) |
| `/types` | `diskSize` descending, then `fileType` ascending (`types.json` pins a tie) |
| hotspot groups | see the classifier §aggregation below |
| `topPaths` within a group | deduped size descending, then path ascending, capped at 5 |

## Squarified treemap (`GET /scans/{id}/treemap`)

Bruls, Huizing & van Wijk (2000), with these binding choices:

1. **Tree building.** Index persisted entries by `path`; children of a node
   are the entries whose `parentPath` equals its path. The layout root is
   the requested `root=` (default: the scan root).
2. **Node size.** Files: `max(diskSize, 1)` (zero-size nodes would vanish
   and break the row math). Directories: `max(child_sum, storedAggregate, 1)`
   — the persisted aggregate wins when the sub-1-MiB remainder was filtered.
3. **Recursion + emission.** Emit this node's rect, then, unless
   `depth == maxDepth` or the node is a file or childless: sort children by
   size DESCENDING (stable — ties keep the path order the children arrived
   in) and squarify them into this node's rect. The root rect is `depth: 0`
   and exactly fills the requested `(0, 0, width, height)`.
4. **Residual synthesis.** If the children under-sum the node by at least
   0.5% of its size (inclusive), append ONE `residual: true` pseudo-child
   at `node.size − child_sum` before the stable sort (so it lands AFTER
   equal-sized real children) and squarify it like any child; emit it as a
   leaf with the PARENT's path and never recurse into it. Below the
   threshold nothing is synthesized (the sliver stays parent background);
   a childless directory gets none. Wire semantics:
   `../06-interchange/wire-format.md`.
5. **Child normalization — against the parent's own size, not the child
   sum**: each child's target area is `(child.size / max(parent.size,
   child_sum)) × parentArea`. For a filtered tree the children cover only
   part of the directory's true bytes; inflating them to fill the parent
   would misrepresent their share — with the residual in the row the items
   sum to the parent's size and coverage is complete.
6. **Squarify proper.** Process sizes (already descending) into rows: with
   `short` = the remaining rect's shorter side, greedily extend the current
   row while the row's WORST aspect ratio does not get worse
   (`ratio <= best` continues; strictly worse closes the row). Worst ratio
   of a row with total `sum` against side `short`:
   `max((short² × size) / sum², sum² / (short² × size))` over each `size`.
   Lay the closed row along the short side — if the remaining rect is at
   least as wide as tall, the row is a vertical strip of width
   `remainingWidth × (rowSum / remainingArea)` with items stacked top-to-
   bottom, each `(size / rowSum) × remainingHeight` tall; otherwise the
   transpose. Subtract the strip from the remaining rect and repeat.
7. Coordinates are absolute within the requested bounds, floating point.
   Cross-implementation comparison MUST normalize floats (the e2e harness
   rounds to 1e-6) — the 17th decimal digit is not part of the contract.

Pinned by: `tests/fixtures/treemap.json` (shape), conformance +
`tests/e2e/run-e2e.sh` (layout at requested size, re-rooting, maxDepth=0).

## The reclaimability classifier (`GET /scans/{id}/hotspots`)

A pure post-pass over a completed scan's FULL walk (before the persistence
filter): input entries + a `now` timestamp → per-entry categories + the
`HotspotsSummary`. Runs ONCE at scan completion; results persist. Categories
and summary field semantics: `../06-interchange/wire-format.md`. Full rule
rationale (every rule was earned in a real cleanup incident):
`docs/reclaimability.md`.

**Constants** (`classify.rs`): dataloaded ratio 8× and floor 1 MiB (only
for rows WITHOUT flags); dormant at ≥ 90 days by default (`olderThan`
overrides per scan); git packs surface above 200 MiB (strict >); plain files
at ≥ 1 GiB (inclusive); 5 top paths per group; git activity files
`logs/HEAD`, `COMMIT_EDITMSG`, `ORIG_HEAD`, `HEAD`.

### The hotspot registry

Hotspot knowledge is a DATA table, one row per hotspot: matcher → category →
hint → tier inputs. Ordering is precedence: the first matching row wins.
Two matching invariants:

- **Component boundaries.** All path matching is path-component aware:
  `node_modules_backup` never matches the `node_modules` rule; a dormant
  `/proj` never marks `/proj-two` stale. Substring matching is a defect.
- **Prove regenerability, don't assume it.** An artifact directory counts
  ONLY beside its detection file: `target/` beside a `Cargo.toml` (or a
  `pom.xml`, or a `build.sbt`), `build/` beside a `package.json` / a
  `build.gradle` / a `CMakeLists.txt` / a `pubspec.yaml`, `vendor/` beside a
  `composer.json` or a `go.mod`. **A detection file outside the scan is
  unprovable: the rule stays silent** (an entry whose parent is not in the
  scan classifies as None — the classifier NEVER stats the filesystem).
  Four names are unambiguous enough to need no proof: `node_modules`,
  `.venv`, `.next`, `DerivedData` (their tier still needs the lockfile).

**Matcher semantics** (paths are absolute `/`-separated strings; all
matching is COMPONENT-boundary aware — never substring):

- `DirNamed(name)` — a directory whose final path component equals `name`.
- `ProjectArtifact(detection…; artifacts…)` — a directory whose name
  matches one of `artifacts` and whose PARENT holds a child matching one of
  `detection`. Patterns are exact names, `*.ext` (a file with that
  extension; `*.py` does not match a file literally named `.py`), or
  `prefix-*`. Parent outside the scan ⇒ no match.
- `DirSuffix(a, b)` — a directory whose path ends with exactly the
  components `…/a/b`.
- `ChildOfDirSuffix(a, b)` — a directory whose PARENT path ends with
  exactly `…/a/b`: each child of `Library/Caches` is its own root, the
  directory itself never is (macOS refuses to rename `~/Library/Caches`
  even for the owner, so a plan path must be a child that can move; an
  unreadable TCC-protected container has no children in the scan and
  nothing under it is planned). A file directly inside is not a cache root.
- `DirNamedWithin(names…, within)` — a directory named one of `names`
  whose parent path contains the component `within` anywhere.
- `GitPackFile(min)` — a FILE named `*.pack` whose parent path ends with
  `…/objects/pack` and whose diskSize is STRICTLY greater than `min`.
- `CloudDataloadedFile` — the per-file override: a file whose `flags`
  contain `dataless`; for a row with `flags: null` (pre-v5), the ratio-and-
  floor heuristic. A `compressed` or `sparse` file is NOT a placeholder.
- `LargeFile(min)` — any otherwise-unmatched file with diskSize ≥ `min`
  (inclusive). MUST stay the LAST row: precedence is registry order.

**Per-row tier inputs.** `lockfiles` (any one beside the detection file
makes a regenerable row `safe`; empty = no lockfile concept = `safe`),
`verify` (the opt-in read-only check, run only when its `requires` lockfile
is the one present), `rebuild` (the `rebuildCost.kind`), and `carve-out`
(a nested root that stays its OWN group inside an enclosing hotspot — the
enclosing group's totals exclude it; model caches and `~/.cache/uv` inside
`~/.cache`).

**The registry, row by row.** `ruleId` and `label` are wire-visible
(HotspotGroup); `hint` and `why` strings are exact bytes — two
implementations must agree on them for the 11-validate parity drill
(backtick characters included). The hint is HUMAN text only; the
machine-actionable safe command rides HotspotGroup's first-class `command`
field (null for advice-only rules). Never parse hints for commands. The
`why` column is the FIRST CLAUSE; the classifier appends the lockfile and
staleness clauses and the final `.` (below).

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

(Markdown-escaping note: hint/why cells use `` `…` `` code fencing; the
VALUE is the text between the outer fences with single-backtick spans kept
verbatim, leading/trailing space trimmed.)

### The classification pass

1. **Directory roots.** Each directory that matches a dir-matcher row
   becomes a hotspot root; nested roots collapse to the OUTERMOST (an inner
   `node_modules` inside `~/.cache` belongs to the `.cache` group) so
   nothing is counted twice — except `carve-out` rows, which keep their own
   root; every entry is still assigned to exactly ONE root (the nearest).
2. **Staleness.** A project root is a directory containing `.git` or any
   row's detection file — except when that marker sits under a hotspot
   root (every package inside `node_modules` ships a `package.json`) or
   under `.git`. Its activity is `now − max(git activity, newest source
   mtime)`: git activity is the newest mtime among `<root>/.git/logs/HEAD`,
   `COMMIT_EDITMSG`, `ORIG_HEAD`, `HEAD` (the reflog moves exactly when
   HEAD does — a commit's mtime IS the commit time; `FETCH_HEAD` and `index`
   are excluded because fetches and `git status` touch them); source mtime
   is over every dated file not under a hotspot root or `.git` (artifact
   mtimes lie). A project is DORMANT when that age is ≥ the threshold (90
   days unless `olderThan`); the boundary is inclusive. A root with neither
   signal is **unverifiable**: never dormant. A `regenerableArtifact` root
   strictly inside a dormant project upgrades to `staleProjectArtifact`.
3. **Lock state per root.** For a row with `lockfiles`: `Present` when one
   sits beside the detection file, else `Missing` (a parent outside the
   scan is `Missing`: unobservable ≠ present). When the scan asked for
   `verifyLocks` and the row's `verify.requires` lockfile is the one
   present, the verify command runs in the project directory:
   exit 0 → `Verified`; non-zero or timeout → `Failed`; tool absent from
   every fixed path → stays `Present`. Rows without `lockfiles` are
   `NotApplicable`.
4. **Per-entry assignment.** Files: the cloud-dataloaded override fires
   FIRST (a placeholder inside `node_modules` still frees ~nothing, so it
   moves to the cloud group rather than inflating the regenerable
   estimate); otherwise the nearest governing directory root; otherwise the
   standalone file rules (git pack, then large-file catch-all). Directories
   inherit their governing root's category — directory rows under a hotspot
   root DO carry categories. Everything else is None.
5. **Tier.** A pure function of (row, effective category, lock state):
   `reviewFirst` / `wontRegenerate` / `cloudDataloaded` → `review`;
   `toolManagedCache` / `modelCache` → `caution`; `cache` → `safe`;
   `regenerableArtifact` / `staleProjectArtifact` → `safe` when the row has
   no lockfile concept or the state is `Present` / `Verified`, else
   `caution`. Staleness never changes the tier — it ranks.
6. **Why.** The row's `why` clause, then by lock state: `Present` → `; a
   lockfile pins the dependency versions`; `Verified` → `; a lockfile pins
   the dependency versions and `<verify command>` confirmed it is current`;
   `Failed` → `; a lockfile is present but verification failed (`<verify
   command>: <reason>`), so a reinstall may resolve different versions`;
   `Missing` → `; no lockfile (<lockfiles, comma-joined>) beside it, so a
   reinstall may resolve different versions`; then, for
   `staleProjectArtifact`, `; the project's newest source edit and git
   activity are at least <threshold> days old`; then `.`.
7. **Rebuild cost.** `kind` is the row's `rebuild`; `estimate` is
   `re-download ≈ <group diskSize>` / `re-compile ≈ <group diskSize> of
   build output` / for `none`: `none — repopulated on demand` (cache and
   regenerable categories) or `not applicable — nothing regenerates this`.
   Sizes format as decimal SI with one decimal (`format_size`).
8. **Aggregation.** Group key = (registry rule, effective category, lock
   state) — the same rule can split into stale and non-stale, and into
   `safe` and `caution`, groups. Only FILES contribute bytes (dirs would
   double-count). Sharing groups — a hardlinked inode `(dev, ino)` with
   `nlink > 1`, or an APFS pure-clone stream `(dev, cloneId)` — dedupe PER
   GROUP for the group's `diskSize`/`topPaths` (one allocation per group,
   the du model), and GLOBALLY for the summary rollups, so blocks shared
   across two hotspots are never promised twice. `listedDiskSize` and
   `fileCount` are the naive per-entry totals. `privateSize` (per group) is
   what deleting the group's paths would ACTUALLY free: a sharing group
   counts only if EVERY reference to it (`nlink`, `CLONE_REFCNT`) lies
   inside the group and inside the scan; ungrouped files contribute the
   kernel's `PRIVATESIZE`. `reclaimEstimate` is Σ privateSize over the five
   reclaimable categories, settled the same way globally; `reviewDiskSize`
   sums globally-deduped disk over reviewFirst + wontRegenerate; the
   cloudDataloaded pair over dataloaded files. Groups sort:
   `staleProjectArtifact` first, then category priority (regenerable, tool-
   managed, cache, model, cloud, review, wont-regenerate), then tier (safe,
   caution, review), then deduped size descending, then `ruleId`.
9. **Tool estimates** (opt-in, after classification). For each probe whose
   rows appear in the summary — `docker system df --format json` →
   `docker-desktop-data` (Σ of every type's `Reclaimable`, decimal units);
   `brew cleanup -n` → the two Cellar rows (the `would free approximately`
   total, else Σ of the `Would remove: … (size)` lines, binary units;
   nothing to remove is 0); `uv cache size` → `uv-cache` (a bare byte
   count) — run ONCE from a fixed absolute path with a scrubbed environment
   and a timeout, and attach `{tool, command, reclaimableBytes, note}` to
   every matching group. Missing tool, non-zero exit, timeout or unparsable
   output ⇒ `toolEstimate` stays null.

Pinned by: `tests/fixtures/hotspots-summary.json` (wire shape),
`tests/fixtures/projects/` (one project per row + decoys: the EXACT set,
walked by the real scanner from `classify.rs` and from the e2e harness —
gate G2), the unit tests in `classify.rs` (every project row with and
without its detection file, every tier/lock state, git-vs-source staleness,
the threshold, the dataless/compressed flags) and `probe.rs` (fixed paths,
the bounded runner, every parser from raw fixture bytes), integration tests
in `rust/phantom-api/tests/test_scans.rs` (tier fields over the wire, the
lockfile mutation, `olderThan` validation and effect, the opt-in verify
lowering a tier), and `tests/e2e/run-e2e.sh` (three-view parity, the
filtered-small-file estimate pin, G2, the request knobs on CLI and MCP).

## Reclaim plans (`POST /scans/{id}/plan`, `POST /plans/{id}/verify`)

**Plan** (`plan.rs::build_plan`): a completed scan's `HotspotsSummary` +
`{maxTier, minBytes}` → `ReclaimPlan`. Walk `groups` IN SUMMARY ORDER (stale
project artifacts first, then category priority, safe before caution, deduped
size descending — the plan never re-sorts). For each group, the FIRST rule
that applies decides:

1. `category` not reclaimable (`cloudDataloaded`, `reviewFirst`,
   `wontRegenerate`) OR `riskTier == review` → skipped, `skipped.review += 1`.
   Unconditional: no request can admit these.
2. `riskTier` above `maxTier` (order `safe < caution`; `review` as a
   requested `maxTier` is clamped to `caution` by the type and refused with
   400 by the API) → `skipped.aboveTier += 1`.
3. `privateSize < minBytes`, or the group has no `topPaths` →
   `skipped.belowMinBytes += 1`.
4. Drop every path that is **held back by git** (`gitignore.rs`): the path
   has a `.git` ancestor (directory or file; the nearest ancestor holding
   one is the work tree root, and `.git` itself never counts) AND no rule
   ignores it. Rules are consulted nearest-first — each `.gitignore` from the
   path's parent up to the work tree root, then `<root>/.git/info/exclude`,
   then git's global excludes file (`core.excludesFile`, else
   `$XDG_CONFIG_HOME/git/ignore`, else `~/.config/git/ignore`); the first
   definite verdict wins (`!` whitelist ⇒ held back; ignore ⇒ candidate); no
   verdict ⇒ held back. Outside any work tree nothing is held back. No path
   left → `skipped.tracked += 1`, no item; some left → the item carries the
   remaining paths and its `why` gains ` N path(s) held back: inside a git
   work tree and not ignored by its .gitignore, so git data rather than a
   cache.` (`expectedFreedBytes` stays the group's `privateSize`.)
4. Otherwise an item: the group's `ruleId`, `label`, `category`, `riskTier`,
   `why`, `command`, `paths = topPaths` (as given, biggest first),
   `expectedFreedBytes = privateSize`, `diskSize`.

`expectedFreedBytes` (plan) = Σ items. `itemCount` = items.length. `planId`
fresh UUID v4; `createdAt` = now. Persisted as the wire JSON; keep-last-25.

**Script** (`plan.rs::script`): `#!/bin/sh`, `set -eu`, `TRASH=$HOME/.Trash/
phantom-<planId>`, `LOG=$TRASH.log`, one `apply '<path>'` line per item path
in plan order, paths POSIX-single-quoted (`'` → `'\''`). `apply` prints
`skip (gone): p` for a missing path, `would move: p` without `PHANTOM_APPLY=1`,
else `mkdir -p $TRASH`, `mv -- p $TRASH/<p with / → _>`, logs `p\tdest`,
prints `moved: p -> dest`. A failed `mv` (macOS refuses to rename
`~/Library/Caches`) prints `FAILED: p — <mv's stderr>`, logs
`p\tFAILED\t<reason>`, and the run CONTINUES (`set -u`, never `set -e`); the
script ends with `moved N, failed N, skipped N (already gone)` (dry run:
`would move N path(s)`) and exits 1 iff `failed > 0`. Never `rm`. Each item's
`command` is a `# alternative:` comment. Pinned: `plan.rs` tests run the
script under `/bin/sh` (dry run touches nothing; apply moves, logs, second
run skips; a read-only parent fails one move, the next still runs, exit 1)
and e2e §14.

**Verify** (`plan.rs::verify_plan`): plan + before scan + before `dir_sizes`
+ after scan + after `dir_sizes` → `ReclaimVerification`. Per item, over its
paths: for each path present in BEFORE's directory rows, `before += size`,
`after += after_size_or_0`; `actualFreedBytes = before − after` (signed,
saturating at i64); all three null when no path was a before directory row.
Root: `actualFreedBytes = before.totalDiskSize − after.totalDiskSize` — unless
some after directory row's path ends with `/.Trash/phantom-<planId>` (the
plan's own Trash folder sits inside the root, as on a home scan, so the moved
bytes are still counted under the root; dogfood 2026-09-09 read −782 MB after
freeing 63 GB), in which case `actualFreedBytes = Σ items.actualFreedBytes`
(nulls skipped, saturating). `shortfallBytes = expected − actual`; `withinTolerance = |shortfall| ≤
0.05 × expected` (expected 0: `actual ≥ 0`). Preconditions (API): after is
`complete`, same root (the diff engine's `same_root`), `after.startedAt ≥
plan.createdAt`; the plan's scan still stored. Pinned: `plan.rs` tests
(tolerance edges, null items, empty plan), `tests/test_plans.rs`, e2e §14
(actual == expected after the script's move).

## Insight (`GET /scans/{id}/explain`, `GET /scans/{id}/stale`, `GET /volume`)

**Per-project activity, recorded at classification** (`classify.rs` step 6):
for every project root the staleness rule evaluated (`project_activity`:
directories holding `.git` or a detection file, not under a hotspot root or
`.git`), emit `{root, lastActivityDays (null == unverifiable), dormant (at
the scan's threshold), artifacts}` where `artifacts` = the kept hotspot roots
whose PARENT is the project root, each with its deduped disk bytes (the
group aggregation's per-root share), sorted by bytes descending then path.
Projects sort by Σ artifact bytes descending, then root. Pinned:
`summary_records_per_project_activity_with_artifacts`, the fixture.

**Explain** (`insight.rs::explain`): entry + summary + the scan's unreadable
sample → `PathExplanation`. `flags` = the entry's wire flag names;
`dataless` = the `dataless` flag. `category` = the entry's persisted
category. `hotspot` = the FIRST group whose `topPaths` contains the path or
a strict ancestor of it (`matchedBy: topPath`); else, when the entry has a
category, the first group of that category (`matchedBy: category`); else
null. `unreadableBelow` = sample entries equal to or strictly under the path
(`/proj-two` is not under `/proj`). `summary` = "<file|directory>: <disk> on
disk (<logical> apparent)"; then "a cloud placeholder…" if dataless, else
"deleting frees <private>" (+ "— the other <shared> is pinned…" when private
< 99% of disk); "+N hard links" when nlink > 1; "classified <category>
(<tier>): <why>" / "classified <category>" / "not a hotspot…"; "N unreadable
entr(y|ies) below it…" when any; joined by "; ", terminated by ".". Human
text — its wording is not a contract.

**Stale** (`insight.rs::stale_projects`): summary.projects filtered to
`lastActivityDays ≥ thresholdDays` (unverifiable never qualifies), sorted by
`artifactDiskSize` descending, then `lastActivityDays` descending, then root.
`thresholdDays` comes from `olderThan` via `parse_older_than` (default 90).

**Volume** (`volume.rs`): `statfs(path)`; `total = f_blocks × f_bsize`,
`free = f_bfree × f_bsize`, `available = f_bavail × f_bsize`, `used = total −
free`; `mountPoint` = `f_mntonname`, `filesystem` = `f_fstypename`. On APFS
these are the CONTAINER's figures. `volumeUsedBytes` =
`getattrlist(path, ATTR_VOL_INFO | ATTR_VOL_SPACEUSED)` (this volume alone;
null when the call fails or the returned length omits the attribute).
`importantUsageBytes` / `opportunisticUsageBytes` = CoreFoundation
`CFURLCopyResourcePropertyForKey` with
`kCFURLVolumeAvailableCapacityForImportantUsageKey` /
`…ForOpportunisticUsageKey` on the path (null when absent);
`purgeableBytes = max(0, importantUsageBytes − availableBytes)`, null when
important is null. With `snapshots`: run `/usr/bin/tmutil listlocalsnapshots
<mountPoint>` (scrubbed env, 10 s); keep only stdout lines matching
`com.apple.TimeMachine.*.local`; `snapshotCount` = their number.

**Hidden space** (`volume.rs::decompose`), given an optional COMPLETED scan
(the API resolves `scanId`; a running/cancelled/failed scan is 409, a scan
whose root `statfs`-resolves to a different `mountPoint` is 400, a root that
no longer exists is tolerated): `scannedBytes = scan.totalDiskSize`;
`unscannedBytes = max(0, volumeUsedBytes − scannedBytes)` (null if either is
null); `otherVolumesBytes = max(0, usedBytes − volumeUsedBytes)` (null if
`volumeUsedBytes` is); `unreadableCount = scan.errorCount`. `otherUserHomes`:
when `/Users` statfs-resolves to the same `mountPoint`, its directory
entries that are directories, not dot-names, not `Shared`, and not the
current `$HOME` (compared canonicalised), each with `readable = access(R_OK
| X_OK) == 0`, sorted by path; else `[]`. `snapshotSuggestion`: when
`snapshotCount > 0`, `"tmutil thinlocalsnapshots <mountPoint> <bytes> 4  #
…"` with `<bytes>` = `purgeableBytes` if known and > 0 else 10 000 000 000;
otherwise null. Nothing here deletes or runs anything beyond the opt-in
tmutil listing. Pinned: `volume-status.json` (arithmetic asserted),
`probes/tmutil-listlocalsnapshots.txt`, `volume.rs` tests, API
`test_insight.rs`, e2e §15, conformance "Volume".

## Growth (`GET /scans/series`)

**Series** (`growth.rs::build`): input = the completed scans of one root,
each with an optional breakdown `[(key, bytes)]` (`None` = the scan
recorded nothing for this `groupBy`); sort by `startedAt` then id, oldest
first → `points`. Keys: take the NEWEST point that has a breakdown, rank
its keys by bytes descending then key ascending, keep the first 10. Each
kept key becomes a line whose value per point is Σ of that key in the
point's breakdown (null when the point has no breakdown). `other` is
appended when any point carries a key outside the kept set or, for
`topLevelDir`, when `totalDiskSize − Σ kept > 0` for any point; its value
is `totalDiskSize − Σ kept` for `topLevelDir` and Σ of the unkept keys
otherwise. Breakdowns per `groupBy`: `total` → `[("total", totalDiskSize)]`;
`category` → the stored hotspot summary's groups summed by
`category.as_str()` (None when no summary); `topLevelDir` → the root's
direct child entries with `isDir` by `name`; `extension` → the
per-extension table with null → `(none)`. Repeated keys sum.

**Forecast** (`growth.rs::forecast`): null when `points.len() < 2` or the
span is 0. `x_i` = days (milliseconds / 86 400 000) since the first point,
`y_i` = `totalDiskSize`; ordinary least squares slope
`Σ(x−x̄)(y−ȳ) / Σ(x−x̄)²` (0 when the denominator is 0), rounded to an
integer `bytesPerDay`. `latestBytes` = the newest point's total.
`availableBytes` = `statfs(root).f_bavail` (null if the root cannot be
stat'd). `daysUntilFull` = `availableBytes / bytesPerDay` when
`bytesPerDay > 0` and available is known, else null; `projectedFullAt` =
`now + daysUntilFull` days (from the unrounded value). **`spanDays` and
`daysUntilFull` are rounded to hundredths on the wire**: a raw f64 does not
survive a JSON round trip byte-for-byte (shortest-repr re-emission vs a
preserved literal), and the three-surface parity gate compares bytes. `caveat` states the assumptions and, when any
point has `totalPrivateSize` null, that those points counted APFS clones
twice. Pinned: `growth.json` (the fixture's forecast is re-derived from its
own points in `fixture_round_trips_and_its_numbers_are_the_arithmetic`),
`growth.rs` tests (exact 10 bytes/day over three points), API
`test_growth.rs`, e2e §16, conformance "Growth".

## Integer limits and the dedup breaker (1.1.0)

**Persist.** Magnitudes (sizes, counts, nlink) are clamped to i64::MAX
before SQLite; identifiers (dev, ino, cloneId) are bit-cast (two's
complement round trip). Pinned: `store.rs`
`hostile_magnitudes_clamp_and_identifiers_round_trip_exactly`.
Consequence: `diff::signed_delta` never saturates on persisted data.

**LinkCharger.** At most 10 000 000 distinct sharing groups are tracked per
aggregation pass. `charges(entry)`: no group → true; group already tracked →
false; set full → true and `saturated = true` (the group stays untracked, so
every later reference is charged too); else insert → true. Pinned:
`format.rs` `link_charger_breaker_charges_untracked_groups_past_the_cap`.

## Template for new entries

```markdown
## <name>

<input> → <output>, stated precisely enough that two implementations agree
byte-for-byte. Include the tie-breaking rules; ties are where divergence
hides. Cite the conformance check or fixture that pins it.
```
