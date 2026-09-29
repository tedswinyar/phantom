#!/usr/bin/env bash
set -euo pipefail
# import-signing-material.sh — one-time, INTERACTIVE, run AS THE BUILD USER on the
# build host. Puts every release credential where the headless jobs expect it and
# writes the release.conf they read (docs/build-server.md).
#
#   import-signing-material.sh \
#       --p12 ~/DeveloperID.p12 \
#       --sparkle-key ~/sparkle_private_key.txt \
#       --notary-key ~/AuthKey_XXXXXX.p8 --notary-key-id XXXXXX \
#       --notary-issuer 11111111-2222-3333-4444-555555555555
#
# What it creates (all under the build user, nothing in any repo):
#   ~/Library/Keychains/phantom-build.keychain-db   Developer ID cert + key
#   ~/.config/phantom-build/keychain-password       0600, read by unlock-build-keychain.sh
#   ~/.config/phantom-build/sparkle_private_key     0600  (NEVER regenerate this key —
#                                                   a new key strands every installed copy)
#   ~/.config/phantom-build/AuthKey.p8              0600
#   ~/.config/phantom-build/release.conf            what build-app/build-dmg/publish read
# Prompts once for the .p12 password. Ends by running scripts/doctor.sh --release.
#
# REUSING ANOTHER PROJECT'S BUILD SERVER (the common case on the MBP, which already
# serves Banshee under the same `builder`): the Developer ID and the App Store
# Connect API key are the TEAM's, not the app's, so only the Sparkle key is
# Phantom's own.
#
#   import-signing-material.sh --reuse-from ~/.config/banshee-build \
#       --sparkle-key ~/phantom_sparkle_private_key.txt
#
# reads SIGNING_IDENTITY, SIGNING_KEYCHAIN, NOTARY_KEY_PATH/ID/ISSUER from that
# directory's release.conf, points at ITS keychain and keychain-password file
# (nothing is copied or re-imported), and writes ~/.config/phantom-build/release.conf
# with KEYCHAIN_PASSWORD_FILE so unlock-build-keychain.sh unlocks the right one.
# --sparkle-key may be omitted to write everything else first; doctor --release then
# names the Sparkle key as the one thing missing.
ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
CFG="$HOME/.config/phantom-build"
KC="$HOME/Library/Keychains/phantom-build.keychain-db"
die() { printf 'import-signing-material: ERROR: %s\n' "$*" >&2; exit 1; }
info() { printf 'import-signing-material: %s\n' "$*"; }

P12="" SPARKLE_KEY="" NKEY="" NKEY_ID="" NISSUER="" REUSE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --p12) P12="$2"; shift 2 ;;
    --sparkle-key) SPARKLE_KEY="$2"; shift 2 ;;
    --notary-key) NKEY="$2"; shift 2 ;;
    --notary-key-id) NKEY_ID="$2"; shift 2 ;;
    --notary-issuer) NISSUER="$2"; shift 2 ;;
    --reuse-from) REUSE="$2"; shift 2 ;;
    *) die "unknown argument $1" ;;
  esac
done
[ "$(id -un)" != "root" ] || die "run as the build user, not root"
umask 077
mkdir -p "$CFG"
chmod 700 "$CFG"

if [ -n "$REUSE" ]; then
  # --- reuse: another project's keychain, password file and notary key -----
  [ -n "$P12$NKEY$NKEY_ID$NISSUER" ] && die "--reuse-from takes the identity and notary key from $REUSE/release.conf; do not also pass --p12/--notary-*"
  [ -r "$REUSE/release.conf" ] || die "no readable release.conf in $REUSE"
  [ -r "$REUSE/keychain-password" ] || die "no keychain-password file in $REUSE (the unlock step needs it)"
  # shellcheck source=/dev/null
  . "$REUSE/release.conf"
  for v in SIGNING_IDENTITY SIGNING_KEYCHAIN NOTARY_KEY_PATH NOTARY_KEY_ID NOTARY_ISSUER_ID; do
    [ -n "${!v:-}" ] || die "$REUSE/release.conf does not set $v"
  done
  [ -f "$SIGNING_KEYCHAIN" ] || die "keychain named in $REUSE/release.conf is missing: $SIGNING_KEYCHAIN"
  [ -r "$NOTARY_KEY_PATH" ] || die "notary key named in $REUSE/release.conf is unreadable: $NOTARY_KEY_PATH"
  security unlock-keychain -p "$(cat "$REUSE/keychain-password")" "$SIGNING_KEYCHAIN"
  security find-identity -v -p codesigning "$SIGNING_KEYCHAIN" | grep -qF "$SIGNING_IDENTITY" \
    || die "'$SIGNING_IDENTITY' does not resolve in $SIGNING_KEYCHAIN"
  info "reusing identity: $SIGNING_IDENTITY"
  info "reusing keychain: $SIGNING_KEYCHAIN (password file $REUSE/keychain-password)"
  info "reusing notary API key $NOTARY_KEY_ID ($NOTARY_KEY_PATH)"
  SPARKLE_LINE="# SPARKLE_PRIVATE_KEY_FILE — MISSING: export Phantom's own key on the dev Mac (generate_keys -x) and re-run with --sparkle-key"
  if [ -n "$SPARKLE_KEY" ]; then
    [ -r "$SPARKLE_KEY" ] || die "cannot read $SPARKLE_KEY"
    /bin/cp -f "$SPARKLE_KEY" "$CFG/sparkle_private_key"; chmod 600 "$CFG/sparkle_private_key"
    SPARKLE_LINE="SPARKLE_PRIVATE_KEY_FILE=\"$CFG/sparkle_private_key\""
  elif [ -r "$CFG/sparkle_private_key" ]; then
    SPARKLE_LINE="SPARKLE_PRIVATE_KEY_FILE=\"$CFG/sparkle_private_key\""
  fi
  cat > "$CFG/release.conf" <<CONF
# Written by scripts/build-server/import-signing-material.sh --reuse-from $REUSE on $(date -u +%Y-%m-%dT%H:%M:%SZ).
# Read by build-app.sh / build-dmg.sh / publish-release.sh via PHANTOM_RELEASE_CONF.
# The identity, keychain and notary key are the team's, shared with that project.
SIGNING_IDENTITY="$SIGNING_IDENTITY"
SIGNING_KEYCHAIN="$SIGNING_KEYCHAIN"
KEYCHAIN_PASSWORD_FILE="$REUSE/keychain-password"
NOTARY_KEY_PATH="$NOTARY_KEY_PATH"
NOTARY_KEY_ID="$NOTARY_KEY_ID"
NOTARY_ISSUER_ID="$NOTARY_ISSUER_ID"
$SPARKLE_LINE
CONF
  chmod 600 "$CFG/release.conf"
  info "wrote $CFG/release.conf"
  info "checking everything with doctor --release"
  PHANTOM_RELEASE_CONF="$CFG/release.conf" "$ROOT_DIR/scripts/doctor.sh" --release
  exit $?
fi

for v in P12 SPARKLE_KEY NKEY NKEY_ID NISSUER; do
  [ -n "${!v}" ] || die "--$(printf '%s' "$v" | tr 'A-Z_' 'a-z-') is required (or --reuse-from <another project's config dir>)"
done
for f in "$P12" "$SPARKLE_KEY" "$NKEY"; do [ -r "$f" ] || die "cannot read $f"; done

# --- the keychain ------------------------------------------------------------
if [ -f "$KC" ]; then
  info "keychain exists: $KC (reusing; delete it to start over)"
  [ -r "$CFG/keychain-password" ] || die "keychain exists but $CFG/keychain-password is missing"
else
  info "creating $KC"
  head -c 32 /dev/urandom | base64 | tr -d '\n=/+' > "$CFG/keychain-password"
  chmod 600 "$CFG/keychain-password"
  security create-keychain -p "$(cat "$CFG/keychain-password")" "$KC"
fi
security unlock-keychain -p "$(cat "$CFG/keychain-password")" "$KC"
security set-keychain-settings "$KC"

# An ssh-only user has an EMPTY user keychain search list (no login keychain was
# ever created for it), and identity validation builds the chain from the SEARCH
# LIST, not from the keychain named on the command line — so `find-identity -v`
# reports the imported Developer ID as invalid even with its intermediate beside
# it (Banshee's build server, 2026-09-13). Put ours in the list, keeping whatever
# is already there.
existing=(); while IFS= read -r k; do [ -n "$k" ] && existing+=("$k"); done < <(security list-keychains -d user | sed 's/^[[:space:]]*"//; s/"$//')
case " ${existing[*]-} " in *" $KC "*) ;; *) security list-keychains -d user -s "$KC" ${existing[@]+"${existing[@]}"} ;; esac

# The same fresh user has none of Apple's intermediates anywhere it can search, and
# a leaf whose chain cannot be built is "not valid" to codesign. Fetch the public
# Developer ID G2 CA (the issuer of every current Developer ID Application cert),
# pinned by SHA-1, and keep it beside the leaf.
G2_SHA1="5B45F61068B29FCC8FFFF1A7E99B78DA9E9C4635"
if ! security find-certificate -c "Developer ID Certification Authority" "$KC" >/dev/null 2>&1; then
  info "importing Apple's Developer ID G2 intermediate"
  tmp="$(mktemp -d)"
  curl -sSfL -o "$tmp/G2.cer" https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer || die "could not download DeveloperIDG2CA.cer"
  got="$(openssl x509 -inform der -in "$tmp/G2.cer" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')"
  [ "$got" = "$G2_SHA1" ] || die "DeveloperIDG2CA.cer fingerprint $got != expected $G2_SHA1"
  security import "$tmp/G2.cer" -k "$KC" >/dev/null
  rm -rf "$tmp"
fi

if security find-identity -v -p codesigning "$KC" | grep -q '"Developer ID Application: '; then
  info "a valid Developer ID Application identity is already in $KC; skipping the .p12 import (re-run)"
else
  info "importing the Developer ID (you will be asked for the .p12 password)"
  printf 'p12 password: '; read -rs P12_PW; echo
  security import "$P12" -k "$KC" -P "$P12_PW" -T /usr/bin/codesign -T /usr/bin/security -T /usr/bin/productsign
  unset P12_PW
  # Let codesign use the key without a UI prompt — the whole point on a headless box.
  security set-key-partition-list -S 'apple-tool:,apple:,codesign:' -s -k "$(cat "$CFG/keychain-password")" "$KC" >/dev/null
fi
IDENTITY="$(security find-identity -v -p codesigning "$KC" | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1)"
[ -n "$IDENTITY" ] || die "no VALID 'Developer ID Application' identity in $KC after import — an 'Apple Development' cert is the wrong export; 'security verify-cert -c <leaf.pem> -p codeSign -k $KC' names a chain problem"
info "identity: $IDENTITY"

# --- files -------------------------------------------------------------------
/bin/cp -f "$SPARKLE_KEY" "$CFG/sparkle_private_key"; chmod 600 "$CFG/sparkle_private_key"
/bin/cp -f "$NKEY" "$CFG/AuthKey.p8"; chmod 600 "$CFG/AuthKey.p8"

# --- release.conf ------------------------------------------------------------
cat > "$CFG/release.conf" <<CONF
# Written by scripts/build-server/import-signing-material.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ).
# Read by build-app.sh / build-dmg.sh / publish-release.sh via PHANTOM_RELEASE_CONF.
SIGNING_IDENTITY="$IDENTITY"
SIGNING_KEYCHAIN="$KC"
KEYCHAIN_PASSWORD_FILE="$CFG/keychain-password"
NOTARY_KEY_PATH="$CFG/AuthKey.p8"
NOTARY_KEY_ID="$NKEY_ID"
NOTARY_ISSUER_ID="$NISSUER"
SPARKLE_PRIVATE_KEY_FILE="$CFG/sparkle_private_key"
CONF
chmod 600 "$CFG/release.conf"
info "wrote $CFG/release.conf"

info "checking everything with doctor --release"
PHANTOM_RELEASE_CONF="$CFG/release.conf" "$ROOT_DIR/scripts/doctor.sh" --release
