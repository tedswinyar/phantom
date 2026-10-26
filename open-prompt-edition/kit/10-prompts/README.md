# 10 — Prompts

Ordered build prompts for rebuilding the system from the kit. All seven
exist (`00-overview` through `06-mcp`); work them in order — each ends with
a runnable exit criterion.

Their shape, if the domain grows another subsystem:

- **Task** — one subsystem, buildable in one focused session
- **Requirements** — numbered, each traceable to a contract section
- **Exit criterion** — a runnable check, not a vibe
- **Sharp edges** — the traps the reference implementation hit, so the
  rebuild doesn't

A prompt that references a contract detail must cite the kit file rather
than restating it; restatements drift.
