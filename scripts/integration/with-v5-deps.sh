#!/usr/bin/env bash
# Run a command with the v5 dependency set in place, restoring the committed (v4) one after.
# Without it the `#![cfg(feature = "v5")]` targets compile to an empty test binary.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE="${REPO_ROOT}/integration"

BACKUP="$(mktemp -d)"
cp "${CRATE}/Cargo.toml" "${BACKUP}/Cargo.toml"
cp "${CRATE}/Cargo.lock" "${BACKUP}/Cargo.lock"
restore() {
  [ -d "${BACKUP}" ] || return 0
  cp "${BACKUP}/Cargo.toml" "${CRATE}/Cargo.toml"
  cp "${BACKUP}/Cargo.lock" "${CRATE}/Cargo.lock"
  rm -rf "${BACKUP}"
}
# A handler does not end the script, hence the explicit exit.
trap restore EXIT
trap 'restore; exit 143' HUP INT TERM

cp "${CRATE}/Cargo.v5.toml" "${CRATE}/Cargo.toml"
cp "${CRATE}/Cargo.v5.lock" "${CRATE}/Cargo.lock"

"$@"
