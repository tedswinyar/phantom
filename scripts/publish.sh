#!/usr/bin/env bash
set -euo pipefail

# publish.sh — DOCUMENTED STUB: pushes the built website and release
# artifacts to wherever this project hosts them. The deploy target is
# project-specific; the template ships the shape, not an account.
#
# Fill in ONE deploy step below and delete the others. Config values come
# from scripts/release.conf (gitignored).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

[ -f "$SCRIPT_DIR/release.conf" ] && . "$SCRIPT_DIR/release.conf"

if [ ! -d "$ROOT_DIR/website" ]; then
  echo "publish: no website layer in this project" >&2
  exit 1
fi

echo "publish: building website"
(cd "$ROOT_DIR/website" && hugo build)

cat <<'EOF' >&2
publish: NOT CONFIGURED — this script is a stub by design.

Pick a deploy step (see website/infra/README.md), wire it in here, and
delete this message:

  # A. S3 + CloudFront (the lineage pattern; needs WEBSITE_BUCKET and
  #    WEBSITE_DISTRIBUTION_ID in release.conf):
  #   aws s3 sync website/public "s3://$WEBSITE_BUCKET" --delete
  #   aws cloudfront create-invalidation --distribution-id "$WEBSITE_DISTRIBUTION_ID" --paths '/*'
  #   aws s3 cp dist/*.dmg "s3://$WEBSITE_BUCKET/releases/"

  # B. GitHub Pages: push website/public to the gh-pages branch and attach
  #    dist/*.dmg to a GitHub release (gh release create).

  # C. Any static host: website/public/ is a plain static site.
EOF
exit 2
