#!/usr/bin/env bash
# Build + publish the RMNG clone template image (template/Dockerfile).
#
#   scripts/publish-template.sh [TEMPLATE_REPO]
#
# Builds with the REPO ROOT as context (the final stage COPYs template/setup/ + the stage
# payloads from it), tags an immutable dated `:YYYYMMDD` + a moving `:latest`, and pushes
# both. Repo defaults to pegasis0/rmng-template; override via the TEMPLATE_REPO env or the
# first positional arg. Rollback = repoint the template reference at a prior dated tag.
set -euo pipefail

REPO="${1:-${TEMPLATE_REPO:-pegasis0/rmng-template}}"
DATE_TAG="$(date +%Y%m%d)"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo ">> building $REPO:$DATE_TAG (+ :latest) from $ROOT"
docker build -f "$ROOT/template/Dockerfile" \
  -t "$REPO:$DATE_TAG" \
  -t "$REPO:latest" \
  "$ROOT"

echo ">> pushing $REPO:$DATE_TAG"
docker push "$REPO:$DATE_TAG"
echo ">> pushing $REPO:latest"
docker push "$REPO:latest"

echo ">> published $REPO:$DATE_TAG and $REPO:latest"
