#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
exec docker buildx build --target test .
