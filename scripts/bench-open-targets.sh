#!/usr/bin/env bash
# Back-compat wrapper. Prefer scripts/stress/run.sh.
exec "$(cd "$(dirname "$0")" && pwd)/stress/run.sh" "${1:-smoke}"
