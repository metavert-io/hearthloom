#!/usr/bin/env bash
# Build and push a Hearthloom container image to a Fly.io registry, for a
# hosting provisioner to run (one machine + volume per tenant).
#
# Configuration comes from `deploy.env` at the repo root (gitignored; copy
# deploy.env.example) or the environment: TENANT_ORG and TENANT_APP are
# required; HEARTHLOOM_REGISTRY_URL / HEARTHLOOM_PROVISIONER_URL are
# optional defaults baked into the image.
#
# Usage: scripts/build-tenant-image.sh [tag]   (default: latest git tag)
# Produces: registry.fly.io/${TENANT_APP}:<tag>. The registry app runs no
# machines.
set -euo pipefail

cd "$(dirname "$0")/.."
if [ -f deploy.env ]; then
  set -a; . ./deploy.env; set +a
fi
: "${TENANT_ORG:?set TENANT_ORG in deploy.env (see deploy.env.example)}"
: "${TENANT_APP:?set TENANT_APP in deploy.env (see deploy.env.example)}"
TAG="${1:-$(git describe --tags --abbrev=0)}"

# Refuse a dirty build: the image must match the tag it claims.
if [ "$(git rev-parse HEAD)" != "$(git rev-parse "$TAG^{commit}")" ]; then
  echo "HEAD is not at tag $TAG — check out the tag (or pass the right one)" >&2
  exit 1
fi
if ! git diff --quiet HEAD -- crates/ Dockerfile Cargo.toml Cargo.lock 2>/dev/null; then
  echo "working tree is dirty under crates/ or Dockerfile — commit first" >&2
  exit 1
fi

# Ensure the registry app exists in the org (idempotent).
fly apps create "$TENANT_APP" --org "$TENANT_ORG" 2>/dev/null \
  || echo "(app $TENANT_APP exists)"

fly deploy --app "$TENANT_APP" --config fly.example.toml --dockerfile Dockerfile \
  --build-only --push --image-label "$TAG" --remote-only \
  --build-arg "HEARTHLOOM_REGISTRY_URL=${HEARTHLOOM_REGISTRY_URL:-}" \
  --build-arg "HEARTHLOOM_PROVISIONER_URL=${HEARTHLOOM_PROVISIONER_URL:-}"

echo
echo "published: registry.fly.io/${TENANT_APP}:${TAG} (org ${TENANT_ORG})"
