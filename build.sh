#!/bin/bash
set -euo pipefail

REGISTRY="${REGISTRY:-ghcr.io}"
OWNER=kryptt
APP=receipt-ledger
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

# Push target, namespaced under $OWNER (e.g. ghcr.io/$OWNER/$APP).
# Override $REGISTRY to push to a different registry.
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
