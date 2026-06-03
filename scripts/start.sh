#!/bin/bash
set -euo pipefail

if [ -z "${KEY_PASS:-}" ]; then
    echo "KEY_PASS must be set before starting the wallet daemon." >&2
    exit 1
fi

if [ -z "${PLATFORM_KEY:-}" ]; then
    echo "PLATFORM_KEY must be set before starting the wallet daemon." >&2
    exit 1
fi

exec wallet "$@"
