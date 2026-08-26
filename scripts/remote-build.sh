#!/usr/bin/env bash
# Builds omarchy-novad on a faster remote machine and copies the resulting
# binaries back for local testing. Source is rsynced to the remote on
# every run; target/ is left on the remote so incremental build caching
# carries over between runs. Only the built binaries come back, not the
# full target/ tree (deps/incremental/.fingerprint stay remote-only).
#
# Why: laptop compiles are slow (NPU/OpenVINO deps included); the remote
# box has more cores/RAM. The remote is for dev iteration speed only --
# this daemon needs the laptop's own NPU/mic/Wayland access to actually
# run, and release builds still go through wherever this project's own
# release process says they should, not this script.
#
# Usage:
#   scripts/remote-build.sh                    # cargo build (debug)
#   scripts/remote-build.sh --release           # cargo build --release
#   scripts/remote-build.sh check               # cargo check
#   scripts/remote-build.sh test
#   scripts/remote-build.sh clippy -- -D warnings
#
# Config (env vars, override if needed):
#   OMARCHY_NOVAD_REMOTE_BUILD_HOST  (default: archmachine)
#   OMARCHY_NOVAD_REMOTE_BUILD_DIR   (default: ~/Work/omarchy-novad)

set -euo pipefail

REMOTE_HOST="${OMARCHY_NOVAD_REMOTE_BUILD_HOST:-archmachine}"
REMOTE_DIR="${OMARCHY_NOVAD_REMOTE_BUILD_DIR:-~/Work/omarchy-novad}"
LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CARGO_CMD="build"
case "${1:-}" in
    check|test|clippy)
        CARGO_CMD="$1"
        shift
        ;;
esac

PROFILE_DIR="debug"
for arg in "$@"; do
    if [[ "$arg" == "--release" ]]; then
        PROFILE_DIR="release"
    fi
done

echo "==> Syncing source to ${REMOTE_HOST}:${REMOTE_DIR}"
ssh "${REMOTE_HOST}" "mkdir -p ${REMOTE_DIR}"
rsync -az --delete \
    --exclude-from="${LOCAL_DIR}/.gitignore" \
    --exclude .git \
    "${LOCAL_DIR}/" "${REMOTE_HOST}:${REMOTE_DIR}/"

echo "==> Running cargo ${CARGO_CMD} $* on ${REMOTE_HOST}"
# shellcheck disable=SC2029
ssh "${REMOTE_HOST}" "source \$HOME/.cargo/env 2>/dev/null; cd ${REMOTE_DIR} && cargo ${CARGO_CMD} $*"

if [[ "${CARGO_CMD}" == "build" ]]; then
    echo "==> Copying binaries back to target/${PROFILE_DIR}/"
    mkdir -p "${LOCAL_DIR}/target/${PROFILE_DIR}"
    rsync -az \
        --exclude deps --exclude build --exclude incremental \
        --exclude '.fingerprint' --exclude examples \
        --exclude '*.d' \
        "${REMOTE_HOST}:${REMOTE_DIR}/target/${PROFILE_DIR}/" \
        "${LOCAL_DIR}/target/${PROFILE_DIR}/"
fi

echo "==> Done."
