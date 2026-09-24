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
#   MODE        scenario (default) = a fresh chain + cluster per test, the isolation the
#               node-killing suites need; suite = one chain + one cluster per test binary
#               (HOPRD_SHARED_CLUSTER=1), ~190 s cheaper per scenario after the first
#   TEST_TARGETS  MODE=suite only: space-separated test binaries (default: TEST_TARGET)
#   SCENARIOS   space-separated test names (default: every test in TEST_TARGET)
#   SCENARIOS_EXCEPT  test names to hold out of that default, e.g. a flaky one
#   SCENARIOS_FIRST   test names to move to the front, when one has to be read before the rest
#   TEST_TARGET test binary to run them from (default: "integration"; "return_path" for
#               the return-path resilience scenarios)
#   TEST_ARGS   extra libtest args, e.g. "--nocapture" to see a passing scenario's own
#               measurements (libtest swallows them otherwise). CI sets this,
#               paired with a narrow RUST_LOG — see .github/workflows/integration.yaml.
#   CARGO_FEATURES  extra cargo flags selecting features, e.g. "--features prof". Test targets
#               behind a non-default feature compile to nothing without it, and cargo reports
#               that as "no test target named X" rather than as a missing feature.
#   others      forwarded to the test (RUST_LOG, HOPRD_PUMP_MBPS, ...) with defaults below
set -euo pipefail

# shellcheck source=scripts/integration/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
it_env
cd "${REPO_ROOT}"

TEST_TARGET="${TEST_TARGET:-integration}"
MODE="${MODE:-scenario}"

case "${MODE}" in
suite)
  # The cluster is brought up once per binary by the first test that asks for it and leaked
  # (integration/src/cluster.rs), so reaping it here is not optional.
  export HOPRD_SHARED_CLUSTER=1
  UNITS="${TEST_TARGETS:-${TEST_TARGET}}"
  ;;
scenario) ;;
*)
  echo "MODE must be 'scenario' or 'suite', got '${MODE}'" >&2
  exit 2
  ;;
esac

# Asked of the binary rather than listed by every caller, so a new scenario runs the moment it is
# written. A caller holds one out by naming it in SCENARIOS_EXCEPT -- enumerating what must NOT run
# keeps the list short and puts the reason next to the name. SCENARIOS still forces an exact set.
if [ "${MODE}" = scenario ] && [ -z "${SCENARIOS:-}" ]; then
  discovered="$(list_scenarios "${TEST_TARGET}")"
  [ -n "${discovered}" ] || {
    echo "no tests found in target '${TEST_TARGET}' -- wrong name, or a feature gate compiled it out" >&2
    exit 1
  }

  # A typo here would hold nothing out and read as a clean run, which is the failure worth being
  # loud about.
  for held in ${SCENARIOS_EXCEPT:-}; do
    case " ${discovered} " in
    *" ${held} "*) ;;
    *)
      echo "SCENARIOS_EXCEPT names '${held}', which is not a test in '${TEST_TARGET}'" >&2
      exit 1
      ;;
    esac
  done

  SCENARIOS=""
  for scenario in ${discovered}; do
    case " ${SCENARIOS_EXCEPT:-} " in
    *" ${scenario} "*) ;;
    *) SCENARIOS="${SCENARIOS}${scenario} " ;;
    esac
  done
  [ -n "${SCENARIOS}" ] || {
    echo "every scenario in '${TEST_TARGET}' is held out by SCENARIOS_EXCEPT" >&2
    exit 1
  }

  # `--list` is alphabetical, which is the wrong order when one scenario has to be read before the
  # rest mean anything. Names in SCENARIOS_FIRST move to the front, in the order given.
  if [ -n "${SCENARIOS_FIRST:-}" ]; then
    rest=""
    for scenario in ${SCENARIOS}; do
      case " ${SCENARIOS_FIRST} " in
      *" ${scenario} "*) ;;
      *) rest="${rest}${scenario} " ;;
      esac
    done
    for first in ${SCENARIOS_FIRST}; do
      case " ${SCENARIOS} " in
      *" ${first} "*) ;;
      *)
        echo "SCENARIOS_FIRST names '${first}', which is not a test in '${TEST_TARGET}'" >&2
        exit 1
        ;;
      esac
    done
    SCENARIOS="${SCENARIOS_FIRST} ${rest}"
  fi
  echo "scenarios in ${TEST_TARGET}: ${SCENARIOS}"
  [ -z "${SCENARIOS_EXCEPT:-}" ] || echo "held out: ${SCENARIOS_EXCEPT}"
fi
trap chain_stop EXIT INT TERM

rc=0
if [ "${MODE}" = suite ]; then
  for target in ${UNITS}; do
    echo "═══ suite ${target}: fresh chain, all scenarios on one cluster ═══"
    chain_start
    cargo_it "${target}" || rc=1
    # Reap before stopping the chain here, and after it below: each binary states its own
    # cluster shape, and the leaked shared cluster must be gone before the next one starts.
    reap_nodes
    chain_stop
  done
else
  for scenario in ${SCENARIOS}; do
    echo "═══ ${scenario}: fresh chain ═══"
    chain_start
    cargo_it "${TEST_TARGET}" "${scenario}" || rc=1
    chain_stop
    reap_nodes
  done
fi
exit "${rc}"
