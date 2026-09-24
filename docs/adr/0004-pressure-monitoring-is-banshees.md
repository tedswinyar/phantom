# ADR-0004: System-pressure monitoring belongs to Banshee, not Phantom

- **Status**: accepted
- **Date**: 2026-08-31
- **Mirror of**: Banshee's ADR-0003 (`~/Code/banshee/docs/adr/0003-sentinel-not-explorer.md`)

## Context

A second app in this portfolio, **Banshee** (`~/Code/banshee`, port 18769, also
stamped from spooky-shell), watches system pressure continuously and alerts when
it rises: CPU saturation, swap thrash, agent-session sprawl, orphaned MCP
processes, corporate monitoring agents, and **free disk space**.

That last one overlaps Phantom. Phantom's Phase 5 is the "Reclaimable track" —
the disk-scan intelligence port, with a hotspot registry, category enum,
source-mtime dormancy analysis and hardlink dedup. If Banshee also cared about
disk in any depth, the two apps would carry two classifiers for the same facts.

The question was settled by asking what job each app does, rather than which
resource each app owns. They turn out to be different jobs with different loops:

|  | Phantom | Banshee |
|---|---|---|
| Question | "Where did my bytes go?" | "Is this machine about to fall over?" |
| Shape | Spatial — treemap, per-path | Temporal — time series, rate of change |
| Cadence | On-demand, expensive full walk | Continuous, cheap sample |
| Verb | Explore | Alarm |

## Decision

**Phantom is the explorer. Banshee is the sentinel.** Phantom keeps all storage
intelligence; Banshee keeps all alarming.

Obligations this places on **Phantom**:

- Phantom does **not** sample continuously and does **not** own a menu bar item.
  A resident watcher for low disk space is not Phantom's to build, however small
  it would be — that is the sentinel role, and putting it here would give the
  user two things to look at that disagree.
- Phantom does **not** grow notification, alert-threshold, or hysteresis
  machinery. If Phantom discovers something a person should be told about
  proactively, the answer is a Banshee dimension, not a Phantom daemon.
- Phantom's `GET /scans/{id}/hotspots` (Phase 5) is a read surface Banshee may
  call. Treat it as having an external consumer from the moment it ships:
  breaking its shape breaks Banshee's read-through, so it follows the same wire
  contract discipline as every other endpoint here.

The reciprocal obligation on **Banshee** is the load-bearing half:
**Banshee never walks the filesystem.** Its disk signals come from `statfs` on
mounted volumes — O(volumes), not O(files). No treemap, no per-path sizes, no
reclaimability categories. When the question becomes "what is taking the space,"
Banshee deep-links here or reads through to this API, attributed.

## Consequences

- Phantom's scope is *protected*, not reduced. The pressure/alerting feature set
  that would otherwise have accreted onto a storage visualizer — thresholds,
  notification permissions, a menu bar, a background agent — lands somewhere else
  entirely, and Phantom's v1.0 plan stays the eight phases it already is.
- Phantom gains its first external API consumer. That is a real constraint on the
  hotspots endpoint's shape and a reason to get it right the first time; it is
  also the first evidence the constellation architecture pays off across apps and
  not just within one.
- **Cost: a user with a full disk needs both apps** — Banshee to be told, Phantom
  to find out why. Accepted deliberately over the alternative of one app that
  does both jobs adequately and neither well.
- If cross-app reads spread beyond this one link, the loopback key-file trust
  model needs revisiting: each app generates its own key, so Banshee must read
  Phantom's key file, which is a wider grant than ADR-0001's trust boundary
  contemplated. Revisit at the second such link, not this one.
- **Escape hatch:** if the split proves confusing in practice, the reversal is to
  let Phantom own low-space *alerting* and drop Banshee's disk dimension — not to
  let Banshee grow a classifier. The no-walk rule is the durable half of this
  decision.
