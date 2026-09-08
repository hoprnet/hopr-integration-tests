#!/usr/bin/env bash
# Run the integration test against a LOCALLY-BUILT chain (anvil + bloklid) instead
# of the bloklid-anvil docker image. Each scenario gets a fresh chain — parity with
# managed container mode, where localcluster starts a throwaway chain per test.
#
# Prereqs (build first): result-hoprd, result-localcluster, result-bloklid, result-foundry.
#   just build          # hoprd + localcluster
#   just build-chain    # bloklid + anvil
#
# Env:
#   SCENARIOS   space-separated test names (default: "zero_hop one_hop")
#   TEST_TARGET test binary to run them from (default: "integration"; "return_path" for
#               the return-path resilience scenarios)
#   TEST_ARGS   extra libtest args, e.g. "--nocapture" to see a passing scenario's own
#               measurements (libtest swallows them otherwise). CI sets this,
#               paired with a narrow RUST_LOG — see .github/workflows/integration.yaml.
#   CARGO_FEATURES  extra cargo flags selecting features, e.g. "--features pix". Test targets
#               behind a non-default feature compile to nothing without it, and cargo reports
#               that as "no test target named X" rather than as a missing feature.
#   others      forwarded to the test (RUST_LOG, HOPRD_PUMP_MBPS, ...) with defaults below
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

export HOPRD_BIN="${HOPRD_BIN:-${REPO_ROOT}/result-hoprd/bin/hoprd}"
export HOPRD_LOCALCLUSTER_BIN="${HOPRD_LOCALCLUSTER_BIN:-${REPO_ROOT}/result-localcluster/bin/hoprd-localcluster}"
export HOPRD_CHAIN_URL="${HOPRD_CHAIN_URL:-http://localhost:8080}"
export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
export HOPRD_PUMP_MBPS="${HOPRD_PUMP_MBPS:-0.5}"

SCENARIOS="${SCENARIOS:-zero_hop one_hop}"
TEST_TARGET="${TEST_TARGET:-integration}"
# Split once into an array. An unquoted ${TEST_ARGS} would be pathname-expanded, so a value
# containing `*` would reach libtest as a list of repository filenames.
read -r -a TEST_ARGS_ARR <<<"${TEST_ARGS:-}"
read -r -a CARGO_FEATURES_ARR <<<"${CARGO_FEATURES:-}"
BLOKLI_API_PORT="${BLOKLI_API_PORT:-8080}"

CHAIN_PID=""
stop_chain() {
  [ -n "${CHAIN_PID}" ] && kill "${CHAIN_PID}" 2>/dev/null || true
  # chain-up.sh traps its own anvil; give the process group a moment to unwind.
  pkill -f "result-bloklid/bin/bloklid" 2>/dev/null || true
  pkill -f "result-foundry/bin/anvil" 2>/dev/null || true
  wait "${CHAIN_PID}" 2>/dev/null || true
  CHAIN_PID=""
}
trap stop_chain EXIT INT TERM

# The cargo test process tears its own cluster down on exit, but localcluster's
# SIGINT→hoprd reaping is async and can lag. Stray nodes from a finished scenario
# steal CPU from the next one — on the crypto-heavy 1-hop path that alone tanks
# arrival. Force-reap and let the machine idle before the next cluster starts.
# Matched against ${HOPRD_BIN} rather than a hardcoded `result-hoprd/bin/hoprd`, because not every
# caller builds through nix: `just pix` needs a PIX-enabled binary, which the flake does not
# produce, so it points HOPRD_BIN at a cargo target directory. A pattern that misses leaves the
# previous scenario's nodes running and stealing CPU from the next one.
reap_nodes_and_settle() {
  # `pkill -f` and `pgrep -f` read their argument as an ERE, and HOPRD_BIN is now an arbitrary
  # path. A `+`, `(` or `[` anywhere in it either stops the pattern matching — the case the
  # comment above is about — or makes pgrep error, which `|| break` then reads as "all gone" and
  # skips the settle entirely. Escaped once here and reused.
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

# chain-up.sh keeps anvil/bloklid output in files rather than on the console, so a
# startup failure would otherwise be a bare "chain died" with no cause. Surface the
# tails at exactly the moment they are worth reading.
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
for scenario in ${SCENARIOS}; do
  echo "═══ ${scenario}: fresh chain ═══"
  start_chain
  if ! nix develop "${HOPRNET_SHELL:-github:hoprnet/hoprnet}" -c \
    cargo test --manifest-path integration/Cargo.toml "${CARGO_FEATURES_ARR[@]}" \
    --test "${TEST_TARGET}" "${scenario}" \
    --no-fail-fast -- --include-ignored --test-threads=1 "${TEST_ARGS_ARR[@]}"; then
    rc=1
  fi
  stop_chain
  reap_nodes_and_settle
done
exit "${rc}"
