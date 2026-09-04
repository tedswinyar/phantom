---
title: "Download"
---

## Requirements

- macOS 14 (Sonoma) or later, Apple Silicon (Intel Macs: build from source below)

## Get Phantom

Download the notarized, stapled DMG from
[GitHub Releases](https://github.com/tedswinyar/phantom/releases/latest) —
open it, drag Phantom to Applications, launch. The app keeps itself current
from the same channel (Sparkle, EdDSA-signed updates; it asks before ever
checking automatically).

Or build from source:

```bash
git clone https://github.com/tedswinyar/phantom.git && cd phantom
make app && open build/Phantom.app
```

## First launch

The app starts its own local API server; nothing to configure. Scan data
lives in `~/Library/Application Support/phantom/` and never leaves your
machine.

To scan protected locations (Desktop, Documents, Mail, most of your home
directory), macOS requires granting Phantom **Full Disk Access** in System
Settings. [Getting started](/docs/getting-started/) has the steps.
