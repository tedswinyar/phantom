#!/usr/bin/env bash
set -euo pipefail
# unlock-build-keychain.sh — make the build user's signing keychain usable by a
# headless job, or lock it again (docs/build-server.md).
#
#   unlock-build-keychain.sh          unlock + put first in the search list
#   unlock-build-keychain.sh --lock   lock it
#
# The keychain and its password file are created by import-signing-material.sh:
#   ~/Library/Keychains/phantom-build.keychain-db
#   ~/.config/phantom-build/keychain-password   (0600)
# A launchd/ssh session has no unlocked login keychain, which is why codesign and
# generate_appcast need a keychain of their own that the job can unlock from a file.
#
# Resolution: PHANTOM_BUILD_KEYCHAIN / PHANTOM_BUILD_KEYCHAIN_PASSWORD_FILE in the
# environment; else SIGNING_KEYCHAIN / KEYCHAIN_PASSWORD_FILE from the release.conf
# named by PHANTOM_RELEASE_CONF (default ~/.config/phantom-build/release.conf) — this
# is how a Phantom build reuses another project's keychain on a shared build server
# (import-signing-material.sh --reuse-from); else the phantom-build defaults.
die() { printf 'unlock-build-keychain: ERROR: %s\n' "$*" >&2; exit 1; }
CONF="${PHANTOM_RELEASE_CONF:-$HOME/.config/phantom-build/release.conf}"
if [ -r "$CONF" ]; then
  # shellcheck source=/dev/null
  . "$CONF"
fi
KC="${PHANTOM_BUILD_KEYCHAIN:-${SIGNING_KEYCHAIN:-$HOME/Library/Keychains/phantom-build.keychain-db}}"
PW_FILE="${PHANTOM_BUILD_KEYCHAIN_PASSWORD_FILE:-${KEYCHAIN_PASSWORD_FILE:-$HOME/.config/phantom-build/keychain-password}}"

[ -f "$KC" ] || die "no keychain at $KC — run scripts/build-server/import-signing-material.sh first"
if [ "${1:-}" = "--lock" ]; then
  security lock-keychain "$KC"
  echo "unlock-build-keychain: locked $KC"
  exit 0
fi
[ -r "$PW_FILE" ] || die "no password file at $PW_FILE"
[ "$(stat -f '%Lp' "$PW_FILE")" = "600" ] || die "$PW_FILE must be mode 0600 (is $(stat -f '%Lp' "$PW_FILE"))"
security unlock-keychain -p "$(cat "$PW_FILE")" "$KC"
# No auto-lock during a long notarization wait; the job locks it explicitly.
security set-keychain-settings "$KC"
# First in the user search list so codesign finds the key without a --keychain
# everywhere; login.keychain stays in the list for everything else.
LOGIN_KC="$HOME/Library/Keychains/login.keychain-db"
if [ -f "$LOGIN_KC" ]; then
  security list-keychains -d user -s "$KC" "$LOGIN_KC"
else
  # An ssh-only build user never gets a login keychain; naming a missing file here
  # would leave the search list in an odd state.
  security list-keychains -d user -s "$KC"
fi
echo "unlock-build-keychain: unlocked $KC; identities:"
security find-identity -v -p codesigning "$KC" | sed 's/^/  /'
