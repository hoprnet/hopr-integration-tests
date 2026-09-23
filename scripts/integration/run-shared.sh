#!/usr/bin/env bash
# EXPERIMENT: one chain + one cluster for a whole test binary, instead of the
# fresh-chain-per-scenario loop in run-binchain.sh.
#
# Same prereqs and env as run-binchain.sh, plus:
#   TEST_TARGETS  space-separated test binaries to run, one cargo invocation each
#                 (default: "integration")
#
# Scenarios run in the order libtest lists them, single-threaded, against the cluster the
# first of them brings up (HOPRD_SHARED_CLUSTER=1). Teardown is here, after the last one of
# the suite: the test process leaks the cluster handle on purpose.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

export HOPRD_BIN="${HOPRD_BIN:-${REPO_ROOT}/result-hoprd/bin/hoprd}"
export HOPRD_LOCALCLUSTER_BIN="${HOPRD_LOCALCLUSTER_BIN:-${REPO_ROOT}/result-localcluster/bin/hoprd-localcluster}"
export HOPRD_CHAIN_URL="${HOPRD_CHAIN_URL:-http://localhost:8080}"
export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
export HOPRD_PUMP_MBPS="${HOPRD_PUMP_MBPS:-0.5}"
export HOPRD_SHARED_CLUSTER=1

TEST_TARGETS="${TEST_TARGETS:-integration}"
BLOKLI_API_PORT="${BLOKLI_API_PORT:-8080}"
read -r -a TEST_ARGS_ARR <<<"${TEST_ARGS:-}"
read -r -a CARGO_FEATURES_ARR <<<"${CARGO_FEATURES:-}"

CHAIN_PID=""
stop_chain() {
  [ -n "${CHAIN_PID}" ] && kill "${CHAIN_PID}" 2>/dev/null || true
  pkill -f "result-bloklid/bin/bloklid" 2>/dev/null || true
  pkill -f "result-foundry/bin/anvil" 2>/dev/null || true
  wait "${CHAIN_PID}" 2>/dev/null || true
  CHAIN_PID=""
}

# The shared cluster is leaked by the test process on purpose, so it is still running when
# cargo exits; reaping it is this script's job.
reap_nodes_and_settle() {
  local bin_re
  bin_re="$(printf '%s' "${HOPRD_BIN}" | sed 's/[][\.^$*+?(){}|]/\\&/g')"
  pkill -f "hoprd-localcluster" 2>/dev/null || true
  pkill -f "${bin_re}" 2>/dev/null || true
  for _ in $(seq 1 30); do
    pgrep -f "${bin_re}|hoprd-localcluster" >/dev/null 2>&1 || break
    sleep 1
  done
  sleep 5
}

teardown() {
  reap_nodes_and_settle
  stop_chain
}
trap teardown EXIT INT TERM

dump_chain_logs() {
  local dir="${CHAIN_DATA_DIR:-/tmp/hopr-chain}"
  for f in bloklid.log anvil.log deployer.log; do
    [ -s "${dir}/${f}" ] || continue
    echo "── last 40 lines of ${dir}/${f} ──" >&2
    tail -40 "${dir}/${f}" >&2
  done
}

start_chain() {
  bash "${REPO_ROOT}/scripts/integration/chain-up.sh" &
  CHAIN_PID=$!
  for _ in $(seq 1 60); do
    curl -sf -X POST "http://localhost:${BLOKLI_API_PORT}/graphql" \
      -H 'content-type: application/json' --data '{"query":"{__typename}"}' >/dev/null 2>&1 && return 0
    kill -0 "${CHAIN_PID}" 2>/dev/null || {
      echo "chain died during startup" >&2
      dump_chain_logs
      return 1
    }
    sleep 2
  done
  echo "chain did not become ready in time" >&2
  dump_chain_logs
  return 1
}

rc=0
for target in ${TEST_TARGETS}; do
  echo "═══ suite ${target}: fresh chain, all scenarios on one cluster ═══"
  start_chain
  if ! nix develop "${HOPRNET_SHELL:-github:hoprnet/hoprnet}" -c \
    cargo test --manifest-path integration/Cargo.toml "${CARGO_FEATURES_ARR[@]}" \
    --test "${target}" \
    --no-fail-fast -- --include-ignored --test-threads=1 "${TEST_ARGS_ARR[@]}"; then
    rc=1
  fi
  # Each binary states its own cluster shape, so a second suite needs a cluster of its own -- and
  # a chain of its own with it: localcluster's node keys are baked in, so a second cluster on the
  # same chain re-registers identities that are already announced and staked there, and its first
  # session then times out.
  reap_nodes_and_settle
  stop_chain
done
exit "${rc}"
