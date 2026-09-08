# Integration throughput test — convenience recipes.
#
# Local quickstart (recommended — binary chain, blokli from flake latest release, no docker):
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
# CI-equivalent (build from main/latest via run.sh):
#   just ci

set shell := ["bash", "-uc"]

# Dev shell providing the rust toolchain. Override with a local checkout for speed:
#   HOPRNET_SHELL=path:../hoprnet just integration
hoprnet := env_var_or_default("HOPRNET_SHELL", "github:hoprnet/hoprnet")

# Chain image for the DOCKER path only (override: `just chain_image=… integration`,
# or set BLOKLID_ANVIL_IMAGE). Floating tag — can drift ahead of the pinned
# binaries; prefer the binary chain (build-chain / integration-binchain) locally.
chain_image := env_var_or_default("BLOKLID_ANVIL_IMAGE", "europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest-rhine")

# Blokli release for the image-free binary chain — keep at the LATEST blokli release
# (override: `just blokli_ref=… build-chain`, or set BLOKLI_REF).
#
# Not merely "latest for its own sake" since v0.14.0: it is the first release whose contract
# addresses carry `service_registry`, which the on-chain service registry added to `HoprChainApi`.
# Against v0.13.0 or earlier, `hoprd-localcluster` exits during bootstrap with "contract addresses
# not a valid JSON: missing field `service_registry`" — before any node starts, so a run fails in
# seconds with nothing about the test in the message.
blokli_ref := env_var_or_default("BLOKLI_REF", "v0.14.0")

# hoprd checkout to build the PIX binaries from. PIX is on `main` since hoprd#91, but the nix
# flake still exposes no PIX-enabled binary — the deposit pool is a non-default cargo feature —
# so `just pix` compiles hoprd from this tree instead of using `just build`.
hoprd_src := env_var_or_default("HOPRD_SRC", "../hoprd")

data_dir := "/tmp/hopr-it"

_default:
    @just --list

# Build local-arch hoprd + hoprd-localcluster binaries from hoprd main (nix,
# Cachix-cached). The localcluster crate is on main. CI builds the pinned rev
# instead (see `just ci` / scripts/integration/run.sh).
build:
    nix build -L github:hoprnet/hoprd#binary-hoprd --out-link result-hoprd
    nix build -L github:hoprnet/hoprd#binary-hoprd-localcluster --out-link result-localcluster

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

# Build the image-free chain: bloklid + blokli-contract-deployer (blokli release)
# and anvil (nixpkgs foundry). Replaces the bloklid-anvil docker image.
build-chain:
    nix build -L 'github:hoprnet/blokli/{{blokli_ref}}#bloklid' --out-link result-bloklid
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
    [ -n "${SCENARIOS}" ] || SCENARIOS='return_paths_should_spread_across_distinct_relayers session_should_survive_return_relayer_loss a_symmetric_session_should_survive_relayer_loss'
    export SCENARIOS TEST_TARGET=return_path
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# End-to-end PIX with edgli as the paying entry (binary chain; manual, NOT run in CI).
# Builds hoprd from HOPRD_SRC (default ../hoprd) because the nix flake has no PIX binary.
# See integration/tests/pix.rs. Optional args = test-name filters.
pix *scenarios: build-chain
    #!/usr/bin/env bash
    set -euo pipefail
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

# PIX under end-user traffic shapes (binary chain; manual, NOT run in CI, hours per full pass).
#
# Same build path as `just pix`, but at a geometry whose cycle is long enough for a traffic shape
# to exist inside it — see integration/src/shapes.rs. Needs a HOPRD_SRC carrying the Exit-side PIX
# fill (hoprnet#8396); the idle scenario measures exactly that, and against a hoprd without it an
# idle cycle strands its deposit by design.
#
# Optional args = test-name filters. The sweep drives this through scripts/integration/pix-sweep.sh.
pix-shapes *scenarios: build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    src="$(cd '{{hoprd_src}}' && pwd)"
    echo "building PIX-enabled hoprd + hoprd-localcluster from ${src}"
    (cd "${src}" && nix develop -c cargo build --release -p hoprd --features strategy-pix-test)
    (cd "${src}" && nix develop -c cargo build --release -p hoprd-localcluster)
    export HOPRD_BIN="${src}/target/release/hoprd"
    export HOPRD_LOCALCLUSTER_BIN="${src}/target/release/hoprd-localcluster"

    grep -qa 'non-anonymous-secp256k1' "${HOPRD_BIN}" || {
      echo "${HOPRD_BIN} was not built with the secp256k1 deposit pool. Rebuild it:" >&2
      echo "    cargo build --release -p hoprd --features strategy-pix-test" >&2
      exit 1
    }
    # The geometry seam these scenarios need. A hoprd-localcluster without it ignores --pix-config
    # and silently runs the demo geometry, which every shape assertion would then be measuring.
    "${HOPRD_LOCALCLUSTER_BIN}" --help 2>&1 | grep -q -- '--pix-config' || {
      echo "${HOPRD_LOCALCLUSTER_BIN} has no --pix-config; HOPRD_SRC is behind the geometry seam." >&2
      exit 1
    }

    # Named explicitly rather than left to a filter: each gets a fresh chain, and the spike runs
    # first because a failure in it means none of the shapes can be read.
    SCENARIOS='{{scenarios}}'
    [ -n "${SCENARIOS}" ] || SCENARIOS='the_profile_geometry_completes_a_cycle an_idle_session_completes_its_cycle_on_exit_fill a_browsing_session_sustains_its_cycles a_download_session_sustains_its_cycles an_upload_session_completes_on_fill a_mixed_session_sustains_its_cycles'
    export SCENARIOS TEST_TARGET=pix_shapes CARGO_FEATURES='--features pix'
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

# CI-equivalent: resolve refs (main/latest or overrides), build, run via run.sh.
ci:
    nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Remove the chain container, stray processes, and temp dirs.
clean:
    -docker ps -aq --filter ancestor='{{chain_image}}' | xargs -r docker rm -f
    -pkill -f result-hoprd/bin/hoprd
    -pkill -f hoprd-localcluster
    -rm -rf '{{data_dir}}' /tmp/hoprd-it-* resolved.env
