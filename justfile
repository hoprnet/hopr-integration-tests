# Integration throughput test — convenience recipes.
#
# Local quickstart (recommended — binary chain, blokli from flake branch release/0.13, no docker):
#   just build-chain            # build blokli(anvil+bloklid) from the flake release
#   just integration-binchain   # build hoprd, run all scenarios against a fresh flake chain
#   just unit                   # fast unit tests (no cluster)
#
# Docker path (LOCAL alternative — CI uses the binary chain; floating :latest tag may drift):
#   just integration            # build binaries, preflight (pull image), run all scenarios
#   just scenario 0-hop         # run a single scenario against a fresh env
#
# Fast iteration (one cluster, many runs — docker path):
#   just cluster-up             # terminal 1: bring up a persistent cluster
#   just attach                 # terminal 2: run scenarios against it
#
# CI-equivalent (resolve refs for a whole release line, build, run every suite):
#   just ci                     # v4 line
#   just ci-v5                  # v5 line, PIX suite included

set shell := ["bash", "-uc"]

# Dev shell providing the rust toolchain. Override with a local checkout for speed:
#   HOPRNET_SHELL=path:../hoprnet just integration
hoprnet := env_var_or_default("HOPRNET_SHELL", "github:hoprnet/hoprnet")

# Chain image for the DOCKER path only (override: `just chain_image=… integration`,
# or set BLOKLID_ANVIL_IMAGE). NOT aligned with the v4 line: the registry has no
# jura tag and no clean `0.13.x` tag for bloklid-anvil (only `0.13.1-commit.*` and
# `-pr.*`), so this floating tag can drift well ahead of the pinned binaries.
# Prefer the binary chain (build-chain / integration-binchain) locally.
chain_image := env_var_or_default("BLOKLID_ANVIL_IMAGE", "europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest-rhine")

# Release line every ref below is derived from: v4 (default) or v5. Mirrors LINE in
# scripts/integration/run.sh — the branch model lives in that file's header. The two lines are
# not mixable: a v4 blokli cannot bootstrap a v5 localcluster, and a v5 edge client pairs with a
# v4 hoprd only by accident.
line := env_var_or_default("LINE", "v4")

# Blokli ref for the image-free binary chain. v0.14.0 is the first release whose contract
# addresses carry `service_registry`, which the v5 chain API requires: against v0.13.0 or earlier
# `hoprd-localcluster` exits during bootstrap with "contract addresses not a valid JSON: missing
# field `service_registry`". Override: `just blokli_ref=… build-chain`, or set BLOKLI_REF.
blokli_ref := env_var_or_default("BLOKLI_REF", if line == "v5" { "v0.14.0" } else { "release/0.13" })

# hoprd branch the binaries are built from (override: `just hoprd_ref=… build`, or HOPRD_REF).
hoprd_ref := env_var_or_default("HOPRD_REF", if line == "v5" { "main" } else { "release/4.1" })

# hoprd checkout to build the PIX binaries from. hoprd's flake does expose
# `binary-hoprd-pix-test`, but for x86_64-linux only — which is what CI uses and what a darwin
# workstation cannot build — so `just pix` compiles hoprd from this tree instead.
hoprd_src := env_var_or_default("HOPRD_SRC", "../hoprd")

data_dir := "/tmp/hopr-it"

_default:
    @just --list

# Build local-arch hoprd + hoprd-localcluster binaries from the selected line's hoprd branch
# (nix, Cachix-cached). CI builds the same branch, or the rev from a merge
# dispatch (see `just ci` / scripts/integration/run.sh).
build:
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd' --out-link result-hoprd
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd-localcluster' --out-link result-localcluster

# Verify docker + nix + pull the chain image (idempotent doctor).
preflight:
    bash scripts/integration/preflight.sh '{{chain_image}}'

# Full local run (managed mode): build → preflight → run both tests.
# Optional args = test-name filters (e.g. `just integration zero_hop`).
integration *filter: build preflight
    #!/usr/bin/env bash
    set -euo pipefail
    export HOPRD_BIN="$PWD/result-hoprd/bin/hoprd"
    export HOPRD_LOCALCLUSTER_BIN="$PWD/result-localcluster/bin/hoprd-localcluster"
    export HOPRD_CHAIN_IMAGE='{{chain_image}}'
    export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
    # Debug-build async setup overflows the default thread stack on x86_64 CI.
    export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
    # Cap send rate so the CPU-constrained runner's packet pool doesn't saturate.
    export HOPRD_PUMP_MBPS="${HOPRD_PUMP_MBPS:-0.5}"
    # Safety-net teardown: remove any chain container left behind (localcluster
    # cleans up on graceful exit; this covers crashes/timeouts).
    trap 'docker ps -aq --filter "ancestor={{chain_image}}" | xargs -r docker rm -f' EXIT
    nix develop {{hoprnet}} -c cargo test --manifest-path integration/Cargo.toml --test integration --no-fail-fast {{filter}} -- --include-ignored --test-threads=1

# Build the image-free chain: bloklid + blokli-contract-deployer (blokli branch)
# and anvil (nixpkgs foundry). Replaces the bloklid-anvil docker image. `--refresh`
# so a moved branch head is picked up instead of nix's cached revision for it.
build-chain:
    nix build -L --refresh 'github:hoprnet/blokli/{{blokli_ref}}#bloklid' --out-link result-bloklid
    nix build -L 'nixpkgs#foundry' --out-link result-foundry

# Full local run WITHOUT docker: build hoprd + the binary chain, then run each
# scenario against a fresh locally-built anvil+bloklid (via --chain-url). Optional
# args = scenarios (default: `zero_hop one_hop`), e.g. `just integration-binchain zero_hop`.
integration-binchain *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    # run-binchain.sh enters the dev shell itself (per scenario), so no outer wrap.
    [ -n '{{scenarios}}' ] && export SCENARIOS='{{scenarios}}'
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# Return-path resilience (binary chain): are replies spread over distinct relayers, and
# does the stream survive one of them dying? Runs its own 5-node cluster — see
# integration/tests/return_path.rs. Optional args = test-name filters.
return-path *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    # Named explicitly rather than left to the default filter: run-binchain.sh gives each
    # scenario a fresh chain, and the kill scenario leaves a dead node behind it.
    SCENARIOS='{{scenarios}}'
    [ -n "${SCENARIOS}" ] || SCENARIOS='return_paths_should_spread_across_distinct_relayers session_should_survive_return_relayer_loss session_should_survive_forward_relayer_loss session_should_survive_common_mode_return_outage a_symmetric_session_should_survive_relayer_loss'
    export SCENARIOS TEST_TARGET=return_path
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# Exit-origination repro (binary chain): does the exit keep originating packets when
# one of its return paths can never be resolved? See integration/tests/exit_origination.rs.
exit-origination: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    export SCENARIOS=exit_should_keep_originating_when_a_return_path_becomes_unresolvable
    export TEST_TARGET=exit_origination
# End-to-end PIX with edgli as the paying entry (binary chain; manual, NOT run in CI).
# Builds hoprd from HOPRD_SRC (default ../hoprd) because the nix flake has no PIX binary.
# See integration/tests/pix.rs. Optional args = test-name filters.
pix *scenarios: build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "PIX is v5-only — run: LINE=v5 just pix" >&2; exit 2; }
    src="$(cd '{{hoprd_src}}' && pwd)"
    echo "building PIX-enabled hoprd + hoprd-localcluster from ${src}"
    # Release rather than debug: debug builds slow packet processing and cryptography enough to
    # distort the SSA cycle pacing the scenarios rest on. Only hoprd needs the feature named —
    # hoprd-localcluster already depends on the same pool unconditionally.
    (cd "${src}" && nix develop -c cargo build --release -p hoprd --features strategy-pix-test)
    (cd "${src}" && nix develop -c cargo build --release -p hoprd-localcluster)
    export HOPRD_BIN="${src}/target/release/hoprd"
    export HOPRD_LOCALCLUSTER_BIN="${src}/target/release/hoprd-localcluster"

    # The deposit pool is a *build-time* choice, and a binary carrying the other one bootstraps
    # normally and then simply never deposits — several minutes into a run. `POOL` in
    # hoprd::strategy is a &str compiled in for exactly this check.
    grep -qa 'non-anonymous-secp256k1' "${HOPRD_BIN}" || {
      echo "${HOPRD_BIN} was not built with the secp256k1 deposit pool. Rebuild it:" >&2
      echo "    cargo build --release -p hoprd --features strategy-pix-test" >&2
      echo "(The pools are mutually exclusive and the binary carries exactly one.)" >&2
      exit 1
    }

    # Named explicitly rather than left to the default filter: run-binchain.sh gives each scenario
    # a fresh chain, and the two here want different entry deposit budgets.
    SCENARIOS='{{scenarios}}'
    [ -n "${SCENARIOS}" ] || SCENARIOS='edgli_entry_deposits_should_be_swept_into_the_exit_safe a_session_should_close_when_the_entry_can_no_longer_deposit'
    export SCENARIOS TEST_TARGET=pix CARGO_FEATURES='--features pix'
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# Run a single test against a fresh env (e.g. `just scenario zero_hop`).
scenario name:
    @just integration '{{name}}'

# Bring up a persistent cluster for iteration (blocks; Ctrl-C to stop). Run in its own terminal.
cluster-up: build preflight
    HOPRD_CHAIN_IMAGE='{{chain_image}}' \
    ./result-localcluster/bin/hoprd-localcluster \
      --size 3 --extra-identities 1 \
      --api-port-base 13000 --p2p-port-base 19000 \
      --api-token test-token-localcluster \
      --hoprd-bin ./result-hoprd/bin/hoprd \
      --data-dir '{{data_dir}}'

# Run tests against the persistent cluster from `cluster-up` (no bring-up).
# Optional args = test-name filters (e.g. `just attach one_hop`).
attach *filter:
    #!/usr/bin/env bash
    set -euo pipefail
    export HOPRD_LOCALCLUSTER_BIN="$PWD/result-localcluster/bin/hoprd-localcluster"
    export HOPRD_CLUSTER_DATA_DIR='{{data_dir}}'
    export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
    export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
    export HOPRD_PUMP_MBPS="${HOPRD_PUMP_MBPS:-0.5}"
    nix develop {{hoprnet}} -c cargo test --manifest-path integration/Cargo.toml --test integration --no-fail-fast {{filter}} -- --include-ignored --test-threads=1

# Fast unit tests (gate + parse logic; no cluster).
unit:
    nix develop {{hoprnet}} -c cargo test --manifest-path integration/Cargo.toml --lib

# Rotsee testnet integration test (manual; NOT run in CI). Needs a pre-funded Gnosis
# identity + reachable exit node via EDGLI_ROTSEE_* (see integration/tests/rotsee.rs).
# Optional args = test-name filters (e.g. `just rotsee rotsee_one_hop`).
rotsee *filter:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
    export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
    nix develop {{hoprnet}} -c cargo test --manifest-path integration/Cargo.toml --test rotsee --release --no-fail-fast {{filter}} -- --ignored --test-threads=1

# Run the Rotsee test against a LOCAL flake binchain cluster (no Gnosis creds needed):
# brings up a standalone cluster, harvests its status into EDGLI_ROTSEE_*, runs the test.
# Needs `just build` + `just build-chain` first. Optional arg = test-name filter.
rotsee-local *filter:
    nix develop {{hoprnet}} -c bash scripts/integration/rotsee-binchain.sh {{filter}}

# Executor-starvation profiling: build with the tracer profile + `prof`, run the
# profiling tests, and collect Perfetto traces (manual; NOT run in CI). Pass
# `--rotsee-only`/`--all` through to the script; see scripts/profile-executor-yield.sh.
profile *args:
    nix develop {{hoprnet}} -c bash scripts/profile-executor-yield.sh {{args}}

# Format + compile-check the crate.
check:
    nix develop {{hoprnet}} -c cargo fmt --manifest-path integration/Cargo.toml
    nix develop {{hoprnet}} -c cargo check --manifest-path integration/Cargo.toml --tests

# What CI checks: fmt --check + clippy (-D warnings). Run before pushing.
lint:
    nix develop {{hoprnet}} -c cargo fmt --manifest-path integration/Cargo.toml --check
    nix develop {{hoprnet}} -c cargo clippy --manifest-path integration/Cargo.toml -p hoprd-integration-test --all-targets -- -D warnings

# CI-equivalent: resolve every ref on the v4 line (or overrides), build, run every suite.
ci:
    nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Same, on the v5 line: hoprd/edge-client `main`, blokli v0.14.0, PIX suite included.
# run.sh swaps `integration/Cargo.v5.toml` in for the run and restores it on exit.
ci-v5:
    LINE=v5 nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Remove the chain container, stray processes, and temp dirs.
clean:
    -docker ps -aq --filter ancestor='{{chain_image}}' | xargs -r docker rm -f
    -pkill -f result-hoprd/bin/hoprd
    -pkill -f hoprd-localcluster
    -rm -rf '{{data_dir}}' /tmp/hoprd-it-* resolved.env
