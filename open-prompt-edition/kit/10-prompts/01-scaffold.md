# Prompt 01 — Scaffold and configuration

## Task

Create the project skeleton in your target language and implement
configuration resolution exactly per `../04-config/config-spec.md`.

## Requirements

1. Read `PHANTOM_PROFILE` (`prod` default, `dev`, `test`); any other
   value is a startup error naming the bad value.
2. Resolve database path, port, and key-file path per the profile table.
   Env overrides (`PHANTOM_DB_PATH`, `PHANTOM_PORT`,
   `PHANTOM_KEY_FILE`) win over profile defaults.
3. Enforce the invariants: test profile requires all three env vars AND
   refuses a database path under the prod data directory; a non-numeric
   port is an error, never a default.
4. Key-file handling: create on first use (UUIDv4, one line, mode 0600,
   parents created); treat empty/whitespace files as absent.

## Exit criterion

A config test suite covering: default profile, each override, unknown
profile rejection, garbage port rejection, the three missing-test-env
errors, and the prod-data refusal. All passing; no network code yet.

## Sharp edges

- Env-var tests mutate process-global state — serialize them (the reference
  uses a mutex around env manipulation).
- "Data dir" and "config dir" are platform conventions; use your platform
  library, do not hardcode `~/Library`.
