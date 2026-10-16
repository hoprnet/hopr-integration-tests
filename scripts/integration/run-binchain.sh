#!/usr/bin/env bash
# Run a test binary against a locally-built chain (anvil + bloklid): one fresh chain and one
# cluster per binary. The cluster is brought up by the first test that asks for it and reused by
# the rest, which is ~170 s of bring-up saved per scenario after the first.
#
# A scenario that must NOT share — one that kills cluster nodes — is isolated by naming it alone,
# so it gets an invocation, a chain and a cluster of its own. See the `return-path` recipe.
#
# Prereqs (build first): result-hoprd, result-localcluster, result-bloklid, result-foundry.
#   just build          # hoprd + localcluster
#   just build-chain    # bloklid + anvil
#
# Env:
#   TEST_TARGETS  space-separated test binaries (default: TEST_TARGET, itself "integration")
#   SCENARIOS     libtest filters, i.e. which scenarios to run (default: every test in the binary)
#   SCENARIOS_EXCEPT  scenarios to hold out, e.g. a flaky one; checked against the binary
#   TEST_ARGS     extra libtest args, e.g. "--nocapture" to see a passing scenario's own
#                 measurements (libtest swallows them otherwise). CI sets this, paired with a
#                 narrow RUST_LOG — see .github/workflows/integration.yaml.
#   CARGO_FEATURES  extra cargo flags selecting features, e.g. "--features prof". Test targets
#                 behind a non-default feature compile to nothing without it, and cargo reports
#                 that as "no test target named X" rather than as a missing feature.
#   HOPR_LOG_DIR  keep everything about this run in a new folder `run-<time>-XXXX` under it:
#                 node logs (hoprd-it-*/), chain logs (chain/), the test output with the edgli
#                 client logs (client-<target>.log), the settings used (run.env) and how the
#                 run ended (run.status). See "Logs of a run" in the README.
#   others        forwarded to the test (RUST_LOG, HOPRD_PUMP_MBPS, ...), defaults in lib.sh
set -euo pipefail

# shellcheck source=scripts/integration/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
it_env
cd "${REPO_ROOT}"

TARGETS="${TEST_TARGETS:-${TEST_TARGET:-integration}}"

# ── Logs of a run (HOPR_LOG_DIR) ─────────────────────────────────────────────

RUN_DIR=""
RUN_STATUS="running"

# Prints a URL without any user:password part.
without_credentials() {
  printf '%s' "${1:-}" | sed -E 's#://[^/@]*@#://#'
}

# The settings this run used, from a fixed list: never the whole environment, never tokens.
write_run_env() {
  local v
  {
    echo "started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "tests_rev=$(git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null)$(git -C "${REPO_ROOT}" diff --quiet HEAD 2>/dev/null || echo '-dirty')"
    for v in RUST_LOG TEST_TARGETS TEST_TARGET SCENARIOS SCENARIOS_EXCEPT TEST_ARGS CARGO_FEATURES \
      HOPRD_PUMP_MBPS HOPRD_CLUSTER_SIZE CHAIN_SHARED; do
      printf '%s=%s\n' "${v}" "${!v:-}"
    done
    echo "HOPRD_CHAIN_URL=$(without_credentials "${HOPRD_CHAIN_URL:-}")"
    # The resolved path names the exact build: a /nix/store hash, or a trace build's target dir.
    echo "hoprd_bin=$(realpath "${HOPRD_BIN}" 2>/dev/null || echo "${HOPRD_BIN}")"
    echo "localcluster_bin=$(realpath "${HOPRD_LOCALCLUSTER_BIN}" 2>/dev/null || echo "${HOPRD_LOCALCLUSTER_BIN}")"
    if [ -n "${HOPRD_CLUSTER_DATA_DIR:-}" ]; then
      echo "# attached cluster: its node logs stay in its own data dir, not in this folder"
      echo "node_logs=${HOPRD_CLUSTER_DATA_DIR}"
    fi
    if [ "${CHAIN_SHARED:-0}" = 1 ]; then
      echo "# shared chain: its logs cover other runs too; see chain-slice.txt for this run's part"
      echo "chain_logs=${CHAIN_DATA_DIR}"
    fi
  } >"${RUN_DIR}/run.env"
}

# Byte size of each shared chain log, so this run's part of it can be cut out later.
chain_log_sizes() {
  local f
  for f in anvil.log bloklid.log deployer.log; do
    f="${CHAIN_DATA_DIR}/${f}"
    if [ -f "${f}" ]; then
      printf '%s %s\n' "${f}" "$(wc -c <"${f}" | tr -d ' ')"
    else
      printf '%s 0\n' "${f}"
    fi
  done
}

# Runs on every exit (success, failure, Ctrl-C): stops the chain this run owns, and writes how the
# run ended next to its logs.
finish_run() {
  local code=$?
  [ "${CHAIN_SHARED:-0}" = 1 ] || chain_stop
  [ -n "${RUN_DIR}" ] || return 0
  [ "${RUN_STATUS}" = running ] && RUN_STATUS=$([ "${code}" = 0 ] && echo passed || echo failed)
  {
    echo "status=${RUN_STATUS}"
    echo "exit_code=${code}"
    echo "finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"${RUN_DIR}/run.status"
  if [ "${CHAIN_SHARED:-0}" = 1 ]; then
    {
      echo "# file  bytes-at-start  bytes-at-end: this run is the part in between"
      paste -d' ' <(cut -d' ' -f1-2 "${RUN_DIR}/.chain-start") <(chain_log_sizes | cut -d' ' -f2)
    } >"${RUN_DIR}/chain-slice.txt" 2>/dev/null || true
    rm -f "${RUN_DIR}/.chain-start"
  fi
  warn_if_trace_capped "${RUN_DIR}"
  echo "logs of this run: ${RUN_DIR} (${RUN_STATUS})"
}

if [ -n "${HOPR_LOG_DIR:-}" ]; then
  mkdir -p "${HOPR_LOG_DIR}"
  RUN_DIR="$(mktemp -d "${HOPR_LOG_DIR}/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXX")"
  # The cluster creates its hoprd-it-* folder under HOPR_LOG_DIR (integration/src/cluster.rs).
  export HOPR_LOG_DIR="${RUN_DIR}"
  # A chain this run starts writes its logs here. A shared chain belongs to the caller.
  [ "${CHAIN_SHARED:-0}" = 1 ] || export CHAIN_DATA_DIR="${RUN_DIR}/chain"
  # Plain text in the files, and the client logs printed for passing tests too.
  export NO_COLOR=1
  case " ${TEST_ARGS:-} " in
  *" --nocapture "*) ;;
  *) export TEST_ARGS="${TEST_ARGS:+${TEST_ARGS} }--nocapture" ;;
  esac
  write_run_env
  [ -n "${HOPRD_TRACE_BUILD_INFO:-}" ] && cp "${HOPRD_TRACE_BUILD_INFO}" "${RUN_DIR}/" 2>/dev/null
  [ "${CHAIN_SHARED:-0}" = 1 ] && chain_log_sizes >"${RUN_DIR}/.chain-start"
  echo "logs of this run: ${RUN_DIR}"
fi

# A typo in SCENARIOS_EXCEPT would hold nothing out and read as a clean run, which is the failure
# worth being loud about. Asked of the binary, so a renamed test is caught too.
if [ -n "${SCENARIOS_EXCEPT:-}" ]; then
  for target in ${TARGETS}; do
    known="$(list_scenarios "${target}")"
    for held in ${SCENARIOS_EXCEPT}; do
      case " ${known} " in
      *" ${held} "*) ;;
      *)
        echo "SCENARIOS_EXCEPT names '${held}', which is not a test in '${target}'" >&2
        exit 1
        ;;
      esac
    done
  done
  echo "held out: ${SCENARIOS_EXCEPT}"
fi
export SKIP_SCENARIOS="${SCENARIOS_EXCEPT:-}"

# CHAIN_SHARED=1: the caller owns one chain for every suite (run.sh), so neither start nor stop it.
trap finish_run EXIT
trap 'RUN_STATUS=interrupted; exit 130' INT TERM

rc=0
for target in ${TARGETS}; do
  if [ "${CHAIN_SHARED:-0}" = 1 ]; then
    echo "═══ ${target}${SCENARIOS:+ (${SCENARIOS})}: shared chain ═══"
  else
    echo "═══ ${target}${SCENARIOS:+ (${SCENARIOS})}: fresh chain ═══"
    chain_start
  fi
  if [ -n "${RUN_DIR}" ]; then
    # stdout and stderr both. With pipefail, a failing test still fails the pipeline.
    # shellcheck disable=SC2086  # SCENARIOS is a deliberate word-split list of libtest filters
    cargo_it "${target}" ${SCENARIOS:-} 2>&1 | tee -a "${RUN_DIR}/client-${target}.log" || rc=1
  else
    # shellcheck disable=SC2086  # SCENARIOS is a deliberate word-split list of libtest filters
    cargo_it "${target}" ${SCENARIOS:-} || rc=1
  fi
  # Reaped before the chain stops: the test process leaks its cluster on purpose
  # (integration/src/cluster.rs), so nothing else will clear it.
  reap_nodes
  [ "${CHAIN_SHARED:-0}" = 1 ] || chain_stop
done
exit "${rc}"
