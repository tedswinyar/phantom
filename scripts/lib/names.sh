# Name-form derivation for init.sh. Sourced, not executed.
#
# From a human project name ("Ghost Note") derive the five sentinel forms:
#   slug       ghost-note     crate names, DB filename, beads prefix, URLs
#   snake      ghost_note     Rust identifiers
#   pascal     GhostNote      Swift targets/types, app name
#   compact    ghostnote      bundle ID segment
#   screaming  GHOST_NOTE     env var prefix
# plus:
#   human      Ghost Note     window titles, website copy
#
# The template's sentinel is "Phantom", whose five forms are textually
# distinct from each other — stamping is therefore blind global replacement
# with no ordering concerns. Tested by scripts/tests/test-names.sh.

# phantom_derive_names <human-name>
# Sets: NAME_HUMAN NAME_SLUG NAME_SNAKE NAME_PASCAL NAME_COMPACT NAME_SCREAMING
# Returns 1 (with a message on stderr) for names it cannot handle.
phantom_derive_names() {
  NAME_HUMAN="$1"

  # Validate: letters, digits, spaces, hyphens; must start with a letter.
  case "$NAME_HUMAN" in
    [A-Za-z]*) ;;
    *)
      echo "names: project name must start with a letter: '$NAME_HUMAN'" >&2
      return 1
      ;;
  esac
  if printf '%s' "$NAME_HUMAN" | LC_ALL=C grep -q '[^A-Za-z0-9 -]'; then
    echo "names: project name may contain only letters, digits, spaces, hyphens: '$NAME_HUMAN'" >&2
    return 1
  fi

  # Normalize word separators to single spaces.
  _names_words="$(printf '%s' "$NAME_HUMAN" | tr '-' ' ' | tr -s ' ' | sed 's/^ *//; s/ *$//')"
  if [ -z "$_names_words" ]; then
    echo "names: project name is empty after normalization" >&2
    return 1
  fi

  NAME_SLUG="$(printf '%s' "$_names_words" | tr '[:upper:]' '[:lower:]' | tr ' ' '-')"
  NAME_SNAKE="$(printf '%s' "$NAME_SLUG" | tr '-' '_')"
  NAME_COMPACT="$(printf '%s' "$NAME_SLUG" | tr -d '-')"
  NAME_SCREAMING="$(printf '%s' "$NAME_SNAKE" | tr '[:lower:]' '[:upper:]')"
  NAME_PASCAL="$(printf '%s' "$_names_words" | awk '{
    out = ""
    for (i = 1; i <= NF; i++) {
      w = $i
      out = out toupper(substr(w, 1, 1)) tolower(substr(w, 2))
    }
    print out
  }')"

  # Refuse names that collide with the sentinel itself: stamping "Spooky
  # App" onto "Phantom" would be a no-op that LOOKS like success.
  if [ "$NAME_SLUG" = "phantom" ] || [ "$NAME_SLUG" = "spooky-shell" ]; then
    echo "names: '$NAME_HUMAN' collides with the template's own names; pick another" >&2
    return 1
  fi

  # Derive a stable default API port in 18000-18999 from the slug, so
  # different stamped projects on one machine avoid each other by default.
  _names_hash="$(printf '%s' "$NAME_SLUG" | cksum | cut -d' ' -f1)"
  NAME_DEFAULT_PORT=$((18000 + _names_hash % 1000))

  unset _names_words _names_hash
  return 0
}
