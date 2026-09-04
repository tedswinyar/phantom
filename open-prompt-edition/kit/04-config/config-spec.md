# Configuration contract

## Environment variables (identical for API, CLI, MCP, app)

| Variable | Meaning | Default |
|---|---|---|
| `PHANTOM_PROFILE` | `prod` \| `dev` \| `test` | `prod` |
| `PHANTOM_API_URL` | Base URL clients call | `http://127.0.0.1:8768` |
| `PHANTOM_API_KEY` | Literal API key (wins over key file) | — |
| `PHANTOM_KEY_FILE` | Path to the key file | per-profile below |
| `PHANTOM_DB_PATH` | Database path (required in `test`) | per-profile below |
| `PHANTOM_PORT` | API port (required in `test`; `0` = ephemeral) | per-profile below |

`PHANTOM_DB_PATH`, `PHANTOM_PORT`, and `PHANTOM_KEY_FILE`, when set, override
the per-profile defaults in EVERY profile — not just `test`. An explicit
override must also work on a headless box where the platform data/config
directories cannot be resolved (resolve platform dirs only when actually
falling back to them).

## Profiles

| Profile | Database | Port | Key file |
|---|---|---|---|
| `prod` | `<data_dir>/phantom/phantom.db` | 8768 | `<config_dir>/phantom/api_key` |
| `dev` | `<data_dir>/phantom/phantom-dev.db` | 8778 | same as prod |
| `test` | `PHANTOM_DB_PATH` **required** | `PHANTOM_PORT` **required** | `PHANTOM_KEY_FILE` **required** |

`<data_dir>`/`<config_dir>` are the platform conventions (macOS: both are
`~/Library/Application Support`).

**Invariants an implementation MUST keep:**

- **Test mode wins**: the test profile refuses any database path under the
  prod data directory, so a mis-set env var cannot touch real data. The check
  judges both the literal path (catches `..` traversal) AND the canonicalized
  deepest-existing ancestor (catches a symlink whose literal path sits outside
  the prod dir but resolves inside it). Either hit is a refusal — this is a
  data-safety backstop, so it errs toward refusing.
- Unknown profile names are an error, not a default.
- A garbage port is an error, not a default.
- A missing required `test` variable is an error naming the variable.

## Port ladder + announcement

If the configured port is busy, try `port+1 … port+9` (10 candidates
including the configured one), then fail with a nonzero exit. Port `0` means
an OS-assigned ephemeral port — no ladder. Whatever binds, print exactly one
line to stdout (flushed) before serving:

```
phantom-api listening on http://127.0.0.1:<port>
```

**The announcement is the truth.** The configured port is a request;
supervisors and harnesses MUST parse this line rather than assume the
configured port. The server binds loopback only. (Context: 8768 is Phantom's
registered port in its local port registry; the ladder 8768–8777 crosses
neighboring registered ports, tolerated ONLY because every supervisor parses
the announcement.)

## Key file

Generated on first API start if missing: a UUIDv4, one line (trailing
newline), mode `0600`, parent directories created. Clients read it as
`trim(file contents)`. An empty or whitespace-only key file is regenerated.
`PHANTOM_API_KEY`, when set, wins over the key file for clients.

**Concurrent-start safety (MUST):** creation uses exclusive create
(O_EXCL / `create_new`). When two server processes race, exactly ONE writes
a key; the loser, on AlreadyExists, ADOPTS the winner's key by re-reading
the file — with a small bounded retry, since the winner may sit between
creating the file and writing the key. Only a file that stays blank through
the retries is treated as the pre-existing-blank case and regenerated.
A create-then-truncate implementation fails this: both processes keep their
own in-memory key while one overwrites the file, and every client reading
the file gets permanent 401s from the other process.
