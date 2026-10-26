#!/usr/bin/env bash
# release-notes.sh — curated highlights for the generated changelog.
#
# git-cliff regenerates CHANGELOG.md from commit subjects on every release,
# so anything written by hand INTO that file is overwritten by the next
# run. Human-readable highlights therefore live beside it, one file per
# version — docs/releases/<version>.md — and are spliced in right under the
# matching `## [<version>] - <date>` header after generation. Re-running is
# idempotent because the changelog is regenerated first.

# insert_highlights <changelog-path> <releases-dir>
# For every `## [X.Y.Z]` header in the changelog with a docs/releases/X.Y.Z.md,
# insert that file's body (a blank line before and after) after the header.
# Versions without a highlights file are left as git-cliff wrote them.
# Prints how many sections were enriched.
insert_highlights() {
  local changelog="$1" dir="$2" tmp count=0 version file
  [ -f "$changelog" ] || { echo "insert_highlights: no changelog at $changelog" >&2; return 1; }
  [ -d "$dir" ] || { printf '0\n'; return 0; }
  tmp="$(mktemp /tmp/phantom-changelog.XXXXXX)"
  : > "$tmp"
  while IFS= read -r line || [ -n "$line" ]; do
    printf '%s\n' "$line" >> "$tmp"
    case "$line" in
      "## ["*"]"*)
        version="${line#\#\# [}"
        version="${version%%]*}"
        file="$dir/$version.md"
        if [ -f "$file" ]; then
          printf '\n' >> "$tmp"
          # Body only: a leading front-matter block or H1 in the highlights
          # file would fight the changelog's own structure.
          sed '/^# /d' "$file" >> "$tmp"
          count=$((count + 1))
        fi
        ;;
    esac
  done < "$changelog"
  mv "$tmp" "$changelog"
  printf '%s\n' "$count"
}
