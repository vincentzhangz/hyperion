#!/usr/bin/env bash
# Orchestrate Hyperion stress/soak runs. Does not start the server.
#
# Usage:
#   scripts/stress/run.sh smoke
#   scripts/stress/run.sh soak
#   scripts/stress/run.sh selftest
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MODE=${1:-${STRESS_MODE:-smoke}}
export STRESS_MODE="$MODE"
export HOST=${HOST:-127.0.0.1:9222}
export HYPERION_AUTH_TOKEN=${HYPERION_AUTH_TOKEN:-}

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
export ARTIFACT_DIR=${ARTIFACT_DIR:-"$ROOT/artifacts/stress-$MODE-$STAMP"}
mkdir -p "$ARTIFACT_DIR"

if ! command -v node >/dev/null 2>&1; then
  echo "node is required (Node.js 20+)" >&2
  exit 1
fi

echo "stress mode=$MODE host=$HOST artifacts=$ARTIFACT_DIR"
node "$ROOT/scripts/stress/driver.mjs"
