# Security

## Privacy stance

**No telemetry. No analytics. No network calls except those listed here.**

- The app, CLI, and MCP server talk only to the local `phantom-api`
  process on 127.0.0.1.
- The API server binds loopback only and never dials out.
- The ONE outbound connection is the Sparkle auto-update check from the app:
  it fetches `https://github.com/tedswinyar/phantom/releases/latest/download/appcast.xml`
  (GitHub Releases; no first-party server exists). Sparkle asks for consent
  before any automatic check; updates are EdDSA-signed (`SUPublicEDKey` in
  Info.plist) on top of Developer ID signing + notarization, so a
  compromised feed cannot ship an installable payload. The CLI, MCP server,
  and API never dial out.

## Trust model

The `X-Api-Key` key file (`0600`, generated locally) is a **local trust
boundary**: it keeps other local users and stray browser JavaScript away
from the API. It is NOT protection against network attackers — the server is
loopback-only by design — nor against anything running as your own user.

Data at rest is an unencrypted SQLite file under
`~/Library/Application Support/phantom/`; protecting it is FileVault's
job, not this app's.

**See [docs/threat-model.md](docs/threat-model.md) for the complete adversary
analysis**, including what we protect, from whom, failure costs, and what
changes the model. When you add a new surface (network sync, sharing, cloud
backup), update BOTH files in the same commit.

## Gates that carry security

There is no hosted CI; the enforcement is the local gate. The pre-push hook
runs `./scripts/verify.sh` on every push (`scripts/install-hooks.sh` installs
it), and a push with a red suite is refused:

- `cargo deny check` (advisories + licenses; `deny.toml`) runs inside
  `verify.sh`'s rust suite when cargo-deny is installed — install it, or the
  advisory gate is decorative.
- `gitleaks` config ships at `.gitleaks.toml` for secret scanning
  (`gitleaks detect` before publishing anything).
- Release binaries are code-signed; DMGs are notarized when
  `scripts/release.conf` is configured (`docs/build-pipeline.md`).

## Reporting a vulnerability

Personal project. Report privately through GitHub — the repository's
**Security** tab → **Report a vulnerability** (private advisories). Do not
open a public issue with exploit details.
