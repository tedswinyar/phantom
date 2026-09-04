# Threat model

Four standing questions, answered for the template slice. Re-answer them
when the domain grows a new surface (network sync, sharing, cloud backup —
each invalidates parts of this analysis).

**This file provides the analysis; [../SECURITY.md](../SECURITY.md) states the
public-facing claims.** When you update this threat model, update SECURITY.md
in the same commit.

## 1. What are we protecting?

The user's notes database
(`~/Library/Application Support/phantom/phantom.db`) — its
confidentiality (personal content), integrity (no silent corruption), and
availability (no data loss).

## 2. From whom?

| Adversary | In scope? | Why |
|---|---|---|
| Other local users on the Mac | **yes** | key-file auth (0600) + loopback-only bind |
| Web pages doing localhost port scans / DNS rebinding | **yes** | requests without `X-Api-Key` get 401; the key never leaves local files |
| Network attackers | no | server binds 127.0.0.1 only; no listening surface |
| Processes running as the same user | no | same-user malware can read the key file; that battle is the OS's |
| The developer (us) shipping a bad migration | **yes** | forward-only migrations + refuse-newer-schema + verified backups (`docs/data-safety.md`) |
| Theft of the powered-off machine | no | FileVault's job; documented in SECURITY.md |

## 3. What are the failure costs?

Data loss > data disclosure for this class of tool. Hence the asymmetry:
backup/migration machinery is tested code, while at-rest encryption is
delegated to the platform.

## 4. What changes the model?

- **Any outbound network call** (update checks, sync): update **THIS FILE** and
  **[../SECURITY.md](../SECURITY.md)** in the same commit. Specifically:
  revisit adversary rows here, update SECURITY.md's "no network calls" list.
- **Multi-device sync**: transport auth, at-rest posture, and conflict
  handling all enter scope — treat as a new model, not an edit. Update both
  files.
- **Attachments/exports**: exported files leave the trust boundary; document
  where they land in both files.

_Template note: keep the table honest. An adversary marked in-scope without
a mechanism enforcing it is marketing, not modeling._
