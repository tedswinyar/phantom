# build-host.sh — where the build host's name and the repositories it serves come
# from, in ONE place. Sourced by the provisioning scripts in this directory.
#
# The build host is a machine on the maintainer's own network, so its name is not
# in the tree. Resolution order for PHANTOM_BUILD_HOST:
#   1. the environment (PHANTOM_BUILD_HOST=… on the command line),
#   2. this machine's own hostname — these scripts run ON the build host, and the
#      name they print is for the developer's `git remote add`.
#
# PHANTOM_REPO is the PRIVATE working repository (owner/name) the relay forwards to
# and the runner registers with — releases build only from it (docs/build-pipeline.md,
# CFBundleVersion monotonicity). PHANTOM_PUBLIC_REPO is where publish-release.sh
# uploads the DMG + appcast and recut-public.sh pushes the squashed public cut.

phantom_build_host() {
  if [ -n "${PHANTOM_BUILD_HOST:-}" ]; then printf '%s\n' "$PHANTOM_BUILD_HOST"; return 0; fi
  hostname
}

phantom_repo() {
  printf '%s\n' "${PHANTOM_REPO:-tedswinyar/phantom-dev}"
}

phantom_public_repo() {
  printf '%s\n' "${PHANTOM_PUBLIC_REPO:-tedswinyar/phantom}"
}
