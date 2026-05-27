#!/bin/bash
set -euo pipefail

REGISTRY=registry.hr-home.xyz
OWNER=kryptt
APP=receipt-ledger
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

# Local-dev push target. Namespaced under $OWNER to mirror the GHCR path
# (ghcr.io/$OWNER/$APP) and the pull-through cache path
# (ghcr.hr-home.xyz/$OWNER/$APP) used for production deploys.
IMG=$REGISTRY/$OWNER/$APP:$VERSION

if docker manifest inspect "$IMG" &>/dev/null; then
  echo "ERROR: $IMG already exists in registry."
  echo "Bump version in Cargo.toml before building."
  exit 1
fi

docker buildx build . -t "$IMG"
docker push "$IMG"

echo "Pushed $IMG"
echo "Update fleet manifest: image: $IMG"
