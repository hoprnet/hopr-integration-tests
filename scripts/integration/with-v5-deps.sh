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

# Optional local overrides so callers (e.g. `just traffic-overhead`) can build the in-process
# entry (`edgli`) against a local `hoprnet` branch and/or local `edge-client` checkout instead of
# upstream `master`.
if [ -n "${EDGLI_PATH:-}" ] || [ -n "${HOPRNET_PATH:-}" ]; then
  rm -f "${CRATE}/Cargo.lock"
fi

if [ -n "${EDGLI_PATH:-}" ]; then
  edgli_abs="$(cd "${EDGLI_PATH}" && pwd)"
  # Match `hopr-strategy` version to the local `edge-client` checkout so only one copy links in.
  strat_ver="$(sed -nE 's/^hopr-strategy[[:space:]]*=[[:space:]]*\{[^}]*version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "${edgli_abs}/Cargo.toml" | head -n 1)"
  if [ -n "${strat_ver}" ]; then
    sed -E "s/^(hopr-strategy[[:space:]]*=[[:space:]]*\{[[:space:]]*version[[:space:]]*=[[:space:]]*\")[^\"]+(\")/\1${strat_ver}\2/" "${CRATE}/Cargo.toml" >"${BACKUP}/Cargo.toml.edited"
    cat "${BACKUP}/Cargo.toml.edited" >"${CRATE}/Cargo.toml"
  fi
  cat >>"${CRATE}/Cargo.toml" <<EOF

[patch."https://github.com/hoprnet/edge-client"]
edgli = { path = "${edgli_abs}" }
EOF
fi

if [ -n "${HOPRNET_PATH:-}" ]; then
  hoprnet_abs="$(cd "${HOPRNET_PATH}" && pwd)"
  cat >>"${CRATE}/Cargo.toml" <<EOF

[patch."https://github.com/hoprnet/hoprnet"]
hopr-crypto-packet = { path = "${hoprnet_abs}/crypto/packet" }
hopr-lib = { path = "${hoprnet_abs}/hopr/hopr-lib" }
hopr-protocol-app = { path = "${hoprnet_abs}/protocols/app" }
hopr-protocol-hopr = { path = "${hoprnet_abs}/protocols/hopr" }
hopr-protocol-pix = { path = "${hoprnet_abs}/protocols/pix" }
hopr-protocol-session = { path = "${hoprnet_abs}/protocols/session" }
hopr-protocol-start = { path = "${hoprnet_abs}/protocols/start" }
hopr-transport = { path = "${hoprnet_abs}/transport/hopr" }
hopr-transport-mixer = { path = "${hoprnet_abs}/transport/mixer" }
hopr-transport-probe = { path = "${hoprnet_abs}/transport/probe" }
hopr-transport-session = { path = "${hoprnet_abs}/transport/session" }
hopr-transport-tag-allocator = { path = "${hoprnet_abs}/transport/tag-allocator" }
EOF
fi

"$@"
