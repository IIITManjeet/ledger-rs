#!/usr/bin/env bash
# Run cargo tests with the env vars sqlx and testcontainers need locally.
#
# Why:
#   - sqlx::query!() needs DATABASE_URL at compile time (to type-check SQL).
#   - testcontainers needs DOCKER_HOST to find OrbStack's socket
#     (OrbStack uses ~/.orbstack/run/docker.sock instead of /var/run/docker.sock).
#
# Usage:
#   scripts/test.sh                      # all workspace tests
#   scripts/test.sh -p ledger-db         # one crate
#   scripts/test.sh --test transactions  # one test file

set -euo pipefail

cd "$(dirname "$0")/.."

export DATABASE_URL="${DATABASE_URL:-postgres://ledger:ledger@localhost:5432/ledger}"

# Auto-detect Docker socket. OrbStack first, then standard Docker Desktop / Linux.
if [ -z "${DOCKER_HOST:-}" ]; then
    if [ -S "$HOME/.orbstack/run/docker.sock" ]; then
        export DOCKER_HOST="unix://$HOME/.orbstack/run/docker.sock"
    elif [ -S "/var/run/docker.sock" ]; then
        :  # testcontainers picks this up by default; no export needed
    fi
fi

echo "DATABASE_URL=$DATABASE_URL"
echo "DOCKER_HOST=${DOCKER_HOST:-/var/run/docker.sock (default)}"

exec cargo test "$@"
