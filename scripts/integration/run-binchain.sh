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
#   others        forwarded to the test (RUST_LOG, HOPRD_PUMP_MBPS, ...), defaults in lib.sh
set -euo pipefail

# shellcheck source=scripts/integration/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
it_env
cd "${REPO_ROOT}"

TARGETS="${TEST_TARGETS:-${TEST_TARGET:-integration}}"

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

trap chain_stop EXIT INT TERM

rc=0
for target in ${TARGETS}; do
  echo "═══ ${target}${SCENARIOS:+ (${SCENARIOS})}: fresh chain ═══"
  chain_start
  # shellcheck disable=SC2086  # SCENARIOS is a deliberate word-split list of libtest filters
  cargo_it "${target}" ${SCENARIOS:-} || rc=1
  # Reaped before the chain stops: the test process leaks its cluster on purpose
  # (integration/src/cluster.rs), so nothing else will clear it.
  reap_nodes
  chain_stop
done
exit "${rc}"
