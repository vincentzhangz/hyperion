#!/usr/bin/env bash
# Run concurrent client-side scrapes from isolated ephemeral profiles.
# Hyperion must already be running.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
export HOST=${HOST:-127.0.0.1:9222}
export HYPERION_AUTH_TOKEN=${HYPERION_AUTH_TOKEN:-}
export PROFILE_COUNT=${PROFILE_COUNT:-20}
export SCRAPE_URL=${SCRAPE_URL:-https://example.com}

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
export ARTIFACT_DIR=${ARTIFACT_DIR:-"$ROOT/artifacts/stress-profiles-$STAMP"}
mkdir -p "$ARTIFACT_DIR"

if ! command -v node >/dev/null 2>&1; then
  echo "node is required (Node.js 20+)" >&2
  exit 1
fi

echo "profile stress count=$PROFILE_COUNT url=$SCRAPE_URL host=$HOST artifacts=$ARTIFACT_DIR"
node "$ROOT/scripts/stress/profile-scrape.mjs"
