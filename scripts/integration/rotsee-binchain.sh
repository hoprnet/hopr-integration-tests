#!/usr/bin/env bash
# Run the Rotsee test (integration/tests/rotsee.rs) against a LOCAL flake-built binchain
# cluster instead of the public testnet. Brings up a standalone localcluster on a fresh
# flake chain, harvests its `hoprd-localcluster status` into the EDGLI_ROTSEE_* env the
# test reads, then runs the test. setup_rotsee uses local network tuning, so loopback
# peers are probed and the fast-chain connector is used.
#
# Prereqs (build first): result-hoprd, result-localcluster, result-bloklid, result-foundry.
#   just build          # hoprd + localcluster
#   just build-chain    # bloklid + anvil (blokli release/0.13)
#
# Run inside the dev shell (needs cargo + jq), e.g. via `just rotsee-local`, or:
#   nix develop github:hoprnet/hoprnet -c bash scripts/integration/rotsee-binchain.sh
#
# Env (optional): filter forwarded to the test (e.g. rotsee_one_hop); RUST_LOG.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

HOPRD_BIN="${HOPRD_BIN:-${REPO_ROOT}/result-hoprd/bin/hoprd}"
LC="${HOPRD_LOCALCLUSTER_BIN:-${REPO_ROOT}/result-localcluster/bin/hoprd-localcluster}"
DATA_DIR="${DATA_DIR:-/tmp/hopr-rotsee-local}"
BLOKLI_API_PORT="${BLOKLI_API_PORT:-8080}"
FILTER="${1:-}"

for bin in "${HOPRD_BIN}" "${LC}" "${REPO_ROOT}/result-bloklid/bin/bloklid" "${REPO_ROOT}/result-foundry/bin/anvil"; do
  [ -x "${bin}" ] || {
    echo "missing ${bin} — run 'just build' and 'just build-chain' first" >&2
    exit 1
  }
done
rm -rf "${DATA_DIR}"

CHAIN_PID=""
CLUSTER_PID=""
cleanup() {
  [ -n "${CLUSTER_PID}" ] && kill -INT "${CLUSTER_PID}" 2>/dev/null || true
  sleep 3
  [ -n "${CHAIN_PID}" ] && kill "${CHAIN_PID}" 2>/dev/null || true
  pkill -f "result-hoprd/bin/hoprd" 2>/dev/null || true
  pkill -f "hoprd-localcluster" 2>/dev/null || true
  pkill -f "result-bloklid/bin/bloklid" 2>/dev/null || true
  pkill -f "result-foundry/bin/anvil" 2>/dev/null || true
  rm -rf "${DATA_DIR}" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "── starting flake chain ──"
bash "${REPO_ROOT}/scripts/integration/lib.sh" chain_up &
CHAIN_PID=$!
chain_ready=false
for _ in $(seq 1 60); do
  curl -sf -X POST "http://localhost:${BLOKLI_API_PORT}/graphql" \
    -H 'content-type: application/json' --data '{"query":"{__typename}"}' >/dev/null 2>&1 && {
    chain_ready=true
    break
  }
  kill -0 "${CHAIN_PID}" 2>/dev/null || {
    echo "chain died during startup" >&2
    exit 1
  }
  sleep 2
done
# Fail here rather than starting the cluster against a chain that never came up — the
# later cluster failure would not point at the unavailable chain.
[ "${chain_ready}" = true ] || {
  echo "chain did not become ready on port ${BLOKLI_API_PORT} within 120s" >&2
  exit 1
}

echo "── starting standalone localcluster (binchain) ──"
HOPRD_CHAIN_URL="http://localhost:${BLOKLI_API_PORT}" "${LC}" \
  --size 3 --extra-identities 1 \
  --api-port-base 13000 --p2p-port-base 19000 \
  --api-token test-token-localcluster \
  --hoprd-bin "${HOPRD_BIN}" \
  --data-dir "${DATA_DIR}" \
  --chain-url "http://localhost:${BLOKLI_API_PORT}" &
CLUSTER_PID=$!

echo "── waiting for cluster to reach 'running' ──"
ready=false
for _ in $(seq 1 120); do
  kill -0 "${CLUSTER_PID}" 2>/dev/null || {
    echo "cluster exited early" >&2
    exit 1
  }
  state="$("${LC}" status --data-dir "${DATA_DIR}" 2>/dev/null | jq -r '.state' 2>/dev/null || true)"
  [ "${state}" = "running" ] && {
    ready=true
    break
  }
  sleep 5
done
[ "${ready}" = true ] || {
  echo "cluster did not reach 'running' in time" >&2
  exit 1
}

# Harvest the funded extra identity + chain endpoint + an exit node from the cluster
# status into the EDGLI_ROTSEE_* contract the test reads.
S="$("${LC}" status --data-dir "${DATA_DIR}")"
# Assigned before export so a failing jq aborts under `set -e` instead of exporting an
# empty value the test only trips over minutes later.
EDGLI_ROTSEE_BLOKLI_URL="$(jq -r '.blokli_url' <<<"${S}")"
EDGLI_ROTSEE_IDENTITY_FILE="$(jq -r '.extras[0].keystore_path' <<<"${S}")"
EDGLI_ROTSEE_IDENTITY_PASSWORD="$(jq -r '.extras[0].password' <<<"${S}")"
EDGLI_ROTSEE_SAFE_ADDRESS="$(jq -r '.extras[0].safe_address' <<<"${S}")"
EDGLI_ROTSEE_MODULE_ADDRESS="$(jq -r '.extras[0].module_address' <<<"${S}")"
EDGLI_ROTSEE_EXIT_NODE="$(jq -r '.nodes[0].address' <<<"${S}")"
export EDGLI_ROTSEE_BLOKLI_URL EDGLI_ROTSEE_IDENTITY_FILE EDGLI_ROTSEE_IDENTITY_PASSWORD
export EDGLI_ROTSEE_SAFE_ADDRESS EDGLI_ROTSEE_MODULE_ADDRESS EDGLI_ROTSEE_EXIT_NODE
export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"

echo "── running rotsee test against local (exit=${EDGLI_ROTSEE_EXIT_NODE}) ──"
cargo test --manifest-path integration/Cargo.toml --test rotsee --release --no-fail-fast \
  ${FILTER:+"${FILTER}"} -- --ignored --test-threads=1
