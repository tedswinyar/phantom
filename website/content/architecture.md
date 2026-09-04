---
title: "Architecture"
---

## A constellation

One Rust API server owns the SQLite database of scans. Everything else — the
SwiftUI app, the `phantom` CLI, the MCP server — is a thin HTTP client of it.

```
        ┌───────────┐   ┌───────────┐   ┌─────────────┐
        │  Mac app  │   │    CLI    │   │  MCP server  │
        └─────┬─────┘   └─────┬─────┘   └──────┬──────┘
              │   HTTP + X-Api-Key (loopback)  │
              └───────────────┬────────────────┘
                       ┌──────┴──────┐
                       │  API  hub   │   ← the ONLY database writer
                       │  + scanner  │
                       └──────┬──────┘
                       ┌──────┴──────┐
                       │   SQLite    │
                       └─────────────┘
```

**Why one writer?** SQLite corruption stories start with two. Auth,
validation, scan lifecycle, and migrations live in exactly one place, and
every client gets them for free. It's also why the size-semantics bug class
is gone: physical size is computed once, in the hub, and every client can
only report what the hub serves.

**Why does the scanner live in the hub?** Scans are asynchronous: starting
one returns an id immediately, progress is polled, cancel flips a flag the
walk actually checks. That lifecycle needs one owner. The treemap layout
lives there too, so the map can be computed at the exact size of whatever
view requests it — and re-rooted server-side when you drill in.

**What gets stored?** Not the raw walk. A scan persists every directory and
every file of 1&nbsp;MiB or more as queryable rows; smaller files fold into
their directory's aggregated totals. The per-type totals and the Reclaimable
classification are computed from the full walk *before* that cut, so the
stored summaries account for everything the walk saw — the row filter bounds
the database, not the truth.

**Why is the port announced, not assumed?** The server climbs a port ladder
when its default (8768) is taken and prints the bound address on stdout;
supervisors — including the Mac app, which bundles and supervises its own
server — parse the announcement. Configured ports are requests, not facts.

**Why a wire-format spec?** The format is implemented twice (Rust and Swift)
and pinned by shared JSON fixtures, so "ISO 8601" can never quietly mean two
different things. On top of that, an end-to-end gate reads the same scan
through the CLI, raw HTTP, and the MCP server and asserts the three views
match byte-for-byte — it runs as a pre-push hook, so parity is enforced, not
promised. The full contract, and everything needed to rebuild this system in
another language, is in the repository's Open Prompt Edition kit.

**Why local-only?** A disk scanner sees everything on your disk, so the
trust story has to be structural: the server binds 127.0.0.1 only, every
request needs a key from a `0600` file in your own Application Support
directory, and the codebase makes zero outbound network calls. The threat
model is written down in the repository (`docs/threat-model.md`,
`SECURITY.md`).

## What Phantom deliberately does not do

- **Delete files.** It classifies, estimates, and suggests the command; you
  run it. Cleanup execution is a post-1.0 question with a much higher safety
  bar.
- **Evict cloud files.** "Free Up Space" is suggested, never performed.
- **Model APFS purgeable space.** Free-space math on APFS moves on its own;
  Phantom documents the honest measurement (`df /System/Volumes/Data`)
  instead of pretending to own it.
- **Auto-update, scan scheduling, scan diffing.** Post-1.0 candidates, not
  v1.0 features.
