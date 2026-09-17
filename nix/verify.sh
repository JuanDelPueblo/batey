#!/usr/bin/env bash
# Canonical source-level verification for Batey.
# Public entrypoint: `nix run .#verify` (see `flake.nix`).
# This script is also runnable directly inside `nix develop`.
# It fails immediately when a stage fails and preserves command output.
set -euo pipefail

# The flake app runs this script from the Nix store, so the script location
# does not point at the checkout. Prefer the invocation directory when it
# looks like the repository root and fall back to the script location
# (direct `./nix/verify.sh` use) otherwise.
ROOT="$PWD"
if [ ! -f "$ROOT/flake.nix" ]; then
  CANDIDATE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  if [ -f "$CANDIDATE/flake.nix" ]; then
    ROOT="$CANDIDATE"
  fi
fi
cd "$ROOT" || exit 1
if [ ! -f flake.nix ]; then
  echo "error: run 'nix run .#verify' from the repository root" >&2
  exit 1
fi

export NG_CLI_ANALYTICS=false
export RUST_BACKTRACE=1

stage() {
  printf '\n=== %s ===\n' "$1"
}

stage "Rust formatting (cargo fmt --check)"
cargo fmt --all -- --check

stage "Rust clippy (-D warnings)"
cargo clippy --all-targets --all-features -- -D warnings

stage "Rust tests (cargo nextest run)"
cargo nextest run --all-features

stage "Frontend dependencies (npm ci)"
npm ci --prefix frontend

stage "Frontend formatting (npm run format:check)"
npm run format:check --prefix frontend

stage "Frontend style linting (npm run lint:styles)"
npm run lint:styles --prefix frontend

stage "Frontend tests (npm test)"
npm test --prefix frontend

stage "Frontend production build (npm run build)"
npm run build --prefix frontend

stage "Fake-backend tests (node --test backend/fake/*.test.mjs)"
node --test backend/fake/*.test.mjs

printf '\nAll verification stages passed.\n'
