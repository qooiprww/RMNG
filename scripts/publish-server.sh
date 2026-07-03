#!/usr/bin/env bash
# Build + publish the RMNG control-server image (root Dockerfile).
#
#   scripts/publish-server.sh [SERVER_REPO]
#
# Builds with the REPO ROOT as context, stamps git-SHA + build-date labels (so the running
# server can show its version + detect updates), tags an immutable dated `:YYYYMMDD` + a
# moving `:latest`, and pushes both. Repo defaults to pegasis0/rmng; override via the
# SERVER_REPO env or the first positional arg. Rollback = repoint the update reference
# (config docker.serverImage) at a prior dated tag.
set -euo pipefail

REPO="${1:-${SERVER_REPO:-pegasis0/rmng}}"
DATE_TAG="$(date +%Y%m%d)"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GIT_SHA="$(cd "$ROOT" && git rev-parse --short HEAD)"
BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo ">> building $REPO:$DATE_TAG (+ :latest) from $ROOT (sha=$GIT_SHA)"
docker build \
  --build-arg "GIT_SHA=$GIT_SHA" \
  --build-arg "BUILD_DATE=$BUILD_DATE" \
  -t "$REPO:$DATE_TAG" \
  -t "$REPO:latest" \
  "$ROOT"

echo ">> pushing $REPO:$DATE_TAG"
docker push "$REPO:$DATE_TAG"
echo ">> pushing $REPO:latest"
docker push "$REPO:latest"

echo ">> published $REPO:$DATE_TAG and $REPO:latest"
