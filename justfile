# Integration throughput test — convenience recipes.
#
# Local quickstart (recommended — binary chain, blokli from flake branch main, no docker):
#   just build-chain            # build blokli(anvil+bloklid) from the flake release
#   just integration-binchain   # build hoprd, run all scenarios against a fresh flake chain
#   just unit                   # fast unit tests (no cluster)
#
# Fast iteration (one cluster, many runs):
#   just cluster-up             # terminal 1: bring up a persistent cluster
#   just attach                 # terminal 2: run scenarios against it
#
# CI-equivalent (resolve refs, build, run the `pr` tier, PIX included):
#   just ci

set shell := ["bash", "-uc"]

# Dev shell providing the rust toolchain. Override with a local checkout for speed:
#   HOPRNET_SHELL=path:../hoprnet just integration
hoprnet := env_var_or_default("HOPRNET_SHELL", "github:hoprnet/hoprnet")

# Blokli branch for the image-free binary chain (override: `just blokli_ref=… build-chain`, or
# set BLOKLI_REF). Needs 0.14+: older contract addresses lack `service_registry`.
blokli_ref := env_var_or_default("BLOKLI_REF", "main")

# hoprd branch the binaries are built from (override: `just hoprd_ref=… build`, or HOPRD_REF).
hoprd_ref := env_var_or_default("HOPRD_REF", "main")

# hoprd checkout to build the PIX binaries (`just pix`) and the TRACE binaries
# (`just integration-trace`) from. hoprd's flake does expose
# `binary-hoprd-pix-test`, but for x86_64-linux only — which is what CI uses and what a darwin
# workstation cannot build — so `just pix` compiles hoprd from this tree instead.
hoprd_src := env_var_or_default("HOPRD_SRC", "../hoprd")

data_dir := "/tmp/hopr-it"

_default:
    @just --list

# Build local-arch hoprd + hoprd-localcluster binaries from the hoprd branch
# (nix, Cachix-cached). CI builds the same branch, or the rev from a merge
# dispatch (see `just ci` / scripts/integration/run.sh).
build:
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd' --out-link result-hoprd
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd-localcluster' --out-link result-localcluster

# Build the image-free chain: bloklid + blokli-contract-deployer (blokli branch)
# and anvil (foundry from the nixpkgs blokli locks). Replaces the bloklid-anvil
# docker image. `--refresh` so a moved branch head is picked up instead of nix's
# cached revision for it.
build-chain:
    nix build -L --refresh 'github:hoprnet/blokli/{{blokli_ref}}#bloklid' --out-link result-bloklid
    nix build -L --inputs-from 'github:hoprnet/blokli/{{blokli_ref}}' 'nixpkgs#foundry' --out-link result-foundry

# Build hoprd + the binary chain, then run the scenarios on one locally-built anvil+bloklid
# and one cluster (via --chain-url). Optional args = scenarios, e.g.
# `just integration-binchain zero_one_two_hop`.
integration-binchain *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    # run-binchain.sh enters the dev shell itself, so no outer wrap.
    [ -n '{{scenarios}}' ] && export SCENARIOS='{{scenarios}}'
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# Like `integration-binchain`, but hoprd and hoprd-localcluster are built from HOPRD_SRC
# (default ../hoprd) with TRACE logs compiled in (see `trace_build` in lib.sh), and each run keeps
# all its logs in its own folder under HOPR_LOG_DIR (default ./logs). Not a performance baseline:
# debug assertions are on.
#   RUST_LOG=info,hopr_protocol_session=trace just integration-trace zero_hop
integration-trace *scenarios: build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    trace_build '{{hoprd_src}}' '{{hoprd_ref}}'
    export HOPR_LOG_DIR="${HOPR_LOG_DIR:-$PWD/logs}"
    [ -n '{{scenarios}}' ] && export SCENARIOS='{{scenarios}}'
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# Return-path resilience (binary chain): are replies spread over distinct relayers, and
# does the stream survive one of them dying? Runs its own 5-node cluster — see
# integration/tests/return_path.rs. Optional args = test-name filters.
#
# One invocation PER SCENARIO, unlike every other suite: these kill cluster nodes, so a shared
# cluster would hand the next scenario a corpse.
return-path *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    it_env
    export TEST_TARGET=return_path HOPRNET_SHELL='{{hoprnet}}'
    scenarios='{{scenarios}}'
    [ -n "${scenarios}" ] || scenarios="$(list_scenarios return_path)"
    # One chain for every scenario: all run 5 nodes, so frozen identities stay warm across them.
    chain_start
    trap chain_stop EXIT
    export CHAIN_SHARED=1
    rc=0
    for scenario in ${scenarios}; do
      SCENARIOS="${scenario}" bash scripts/integration/run-binchain.sh || rc=1
    done
    exit "${rc}"

# Exit-origination repro (binary chain): does the exit keep originating packets when
# one of its return paths can never be resolved? See integration/tests/exit_origination.rs.
exit-origination: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    export TEST_TARGET=exit_origination
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/run-binchain.sh

# A relay that acknowledges everything and delivers nothing toward the exit (GNO-1183): does a
# 1-hop session avoid it? See integration/tests/dead_last_hop.rs. Also runs in the `pr` tier of CI.
# `EDGLI_REF=<branch|sha>` runs an unmerged edge-client; `NETWORK_GRAPH_REF=<branch>` also patches
# an unreleased hopr-network-graph from hoprnet/hopr-impls (Cargo ignores edge-client's own patch).
# Both need `gh` logged in, to resolve the ref and read edge-client's manifest.
dead-last-hop: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    # Inherited, it would replace TEST_TARGET in run-binchain.sh and run another suite.
    unset TEST_TARGETS
    export TEST_TARGET=dead_last_hop
    export HOPRNET_SHELL='{{hoprnet}}'
    source scripts/integration/lib.sh
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      if [ -n "${NETWORK_GRAPH_REF:-}" ]; then
        printf "\n[patch.crates-io]\nhopr-network-graph = { git = \"https://github.com/hoprnet/hopr-impls\", branch = \"%s\" }\n" \
          "${NETWORK_GRAPH_REF}" >>integration/Cargo.toml
      fi
      if [ -n "${EDGLI_REF:-}" ]; then
        pin_edgli "$(resolve_sha hoprnet/edge-client "${EDGLI_REF}")"
      fi
      exec bash scripts/integration/run-binchain.sh'

# gnosis_vpn-testenv's diagnostic tests on gnosis_vpn-client's session (nightly tier). See
# integration/tests/wireguard.rs. Optional args = test-name filters, e.g. `just wireguard short_tcp`.
wireguard *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    unset TEST_TARGETS
    export TEST_TARGET=wireguard HOPRNET_SHELL='{{hoprnet}}'
    [ -n '{{scenarios}}' ] && export SCENARIOS='{{scenarios}}'
    source scripts/integration/lib.sh
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      exec bash scripts/integration/run-binchain.sh'

# End-to-end PIX with edgli as the paying entry (binary chain; CI's `pr` tier runs it too).
# Builds hoprd from HOPRD_SRC (default ../hoprd) because the nix flake has no PIX binary.
# See integration/tests/pix.rs. Optional args = test-name filters.
#
# The Entry is linked into the test binary, so it is whatever edgli the manifest names. Like
# `just tier`, this and the other PIX recipes pin edgli to edge-client's `main` head (`EDGLI_REF`
# overrides it) rather than the committed lock, which trails it; `gh` must be logged in.
pix *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    export SCENARIOS='{{scenarios}}' TEST_TARGET=pix HOPRNET_SHELL='{{hoprnet}}'
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    # The pin rewrites the committed manifest and lock; they are restored once the run ends.
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      exec bash scripts/integration/run-binchain.sh'

# PIX under end-user traffic shapes (binary chain, hours per full pass; CI runs the mixed shape on
# PRs and the rest nightly).
#
# Same build path as `just pix`, but at a geometry whose cycle is long enough for a traffic shape
# to exist inside it — see integration/src/shapes.rs. Needs a HOPRD_SRC carrying the Exit-side PIX
# fill (hoprnet#8396); the idle scenario measures exactly that, and against a hoprd without it an
# idle cycle strands its deposit by design.
#
# Optional args = test-name filters. Pins the Entry's edgli like `just pix` does.
pix-shapes *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"
    pix_check_localcluster "${HOPRD_LOCALCLUSTER_BIN}"

    export SCENARIOS='{{scenarios}}'
    export TEST_TARGET=pix_shapes HOPRNET_SHELL='{{hoprnet}}'
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      exec bash scripts/integration/run-binchain.sh'

# Run a CI tier locally: the same suites as `run.sh` (lib.sh `run_tier`), from the binaries built
# here rather than resolved refs. Also builds the PIX hoprd + localcluster from HOPRD_SRC, a hoprd
# git checkout, since the flake's PIX binary is x86_64-linux only. See docs/test-matrix.md.
#   HOPRD_SRC=../hoprd-main just tier pr
#   HOPRD_SRC=../hoprd-main just tier nightly
tier name="pr": build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    it_env
    export TIER='{{name}}' HOPRNET_SHELL='{{hoprnet}}' PIX_BIN=""
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    nodes_bin="${HOPRD_BIN}"
    pix_build '{{hoprd_src}}'
    pix_check_hoprd "${HOPRD_BIN}"
    PIX_BIN="${HOPRD_BIN}" HOPRD_BIN="${nodes_bin}"
    chain_start
    trap chain_stop EXIT
    # Against edge-client's `main` head, like CI: the committed lock trails it.
    with_restored_manifest bash -c 'source scripts/integration/lib.sh; pin_deps; run_tier'

# PIX when half the return relays die mid-cycle (binary chain; CI's nightly tier runs it too;
# about ten minutes per scenario at the default geometry, ~20 for the one that sustains three
# cycles). See integration/tests/pix_relay_loss.rs and docs/pix-relay-loss.md. Same build path as
# `just pix`.
#
# One cluster PER SCENARIO, like `return-path` and for the same reason: each kills a cluster node.
# They share one chain, started once. `HOPRD_PIX_*` (surplus, geometry, SURB buffer, rate) are
# forwarded to the test; docs/pix-relay-loss.md lists them. Optional args = test-name filters.
#
# Pins the Entry's edgli like `just pix` does, once, so every scenario runs the same Entry.
pix-relay-loss *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    it_env
    export TEST_TARGET=pix_relay_loss HOPRNET_SHELL='{{hoprnet}}'
    export SCENARIO_FILTERS='{{scenarios}}'
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    chain_start
    trap chain_stop EXIT
    export CHAIN_SHARED=1
    # One pin for the listing and every scenario; see `pix`.
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      scenarios="${SCENARIO_FILTERS}"
      [ -n "${scenarios}" ] || scenarios="$(list_scenarios pix_relay_loss)"
      rc=0
      for scenario in ${scenarios}; do
        SCENARIOS="${scenario}" bash scripts/integration/run-binchain.sh || rc=1
      done
      exit "${rc}"'

# PIX Session resume: a client forced to reconnect re-opens its Session under the same pseudonym and
# keeps its paid cycle (binary chain; nightly tier). See integration/tests/pix_resume.rs.
# A few minutes per scenario at the test's own small geometry. Same build path as `just pix`.
#
# Needs a hoprd/localcluster that knows `resume_window` (hoprd#293) and a hoprnet carrying the
# resume window (hoprnet#8711): the test states a 10 s window through `--pix-config`, and
# localcluster rejects keys it does not know. One cluster PER SCENARIO on one shared chain, like
# `pix-relay-loss`. Optional args = test-name filters. Pins the Entry's edgli like `just pix` does.
pix-resume *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"
    pix_check_localcluster "${HOPRD_LOCALCLUSTER_BIN}"
    pix_check_resume_window "${HOPRD_LOCALCLUSTER_BIN}"

    it_env
    export TEST_TARGET=pix_resume HOPRNET_SHELL='{{hoprnet}}'
    export SCENARIO_FILTERS='{{scenarios}}'
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    chain_start
    trap chain_stop EXIT
    export CHAIN_SHARED=1
    # One pin for the listing and every scenario; see `pix`.
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      scenarios="${SCENARIO_FILTERS}"
      [ -n "${scenarios}" ] || scenarios="$(list_scenarios pix_resume)"
      rc=0
      for scenario in ${scenarios}; do
        SCENARIOS="${scenario}" bash scripts/integration/run-binchain.sh || rc=1
      done
      exit "${rc}"'

# PIX at the DEPLOYED configuration (binary chain; manual, NOT run in CI, about ten minutes).
#
# Same build path as `just pix`, but at the geometry hoprnet#8519 ships as its default —
# `4096 x (32 + 79)`, a cycle of 454 656 packets and a 629.6 MiB quota — with the Exit's upstream
# PIX defaults, rather than the demo geometry `just pix` uses. No fault is injected: this is the
# deployed-configuration control for `just pix-relay-loss`. See integration/tests/pix_prod.rs.
# Optional args = test-name filters. Pins the Entry's edgli like `just pix` does.
pix-prod *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    export SCENARIOS='{{scenarios}}' TEST_TARGET=pix_prod HOPRNET_SHELL='{{hoprnet}}'
    # Release, not debug, whatever TEST_PROFILE says: the Entry is linked into this binary and
    # generating 4096 polynomials per cycle is CPU-bound enough that a debug build cannot keep up
    # with the Exit's commitment deadline — measured, it stretched the SSA commitment past
    # max_ssa_delivery_time and every cycle failed.
    export TEST_PROFILE=release
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    with_restored_manifest bash -c '
      set -euo pipefail
      source scripts/integration/lib.sh
      pin_deps
      exec bash scripts/integration/run-binchain.sh'

# Run a single test against a fresh env (e.g. `just scenario zero_one_two_hop`).
scenario name:
    @just integration-binchain '{{name}}'

# Bring up a persistent cluster on a fresh binary chain (blocks; Ctrl-C to stop).
# Run in its own terminal, then drive it from another with `just attach`.
cluster-up: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    it_env
    trap 'chain_stop; reap_nodes' EXIT INT TERM
    chain_start
    cluster_up '{{data_dir}}'
    cluster_wait '{{data_dir}}'
    echo "cluster ready — run \`just attach\` in another terminal; Ctrl-C here to tear down"
    wait "${CLUSTER_PID}"

# Run tests against the persistent cluster from `cluster-up` (no bring-up).
# Optional args = test-name filters (e.g. `just attach zero_one_two_hop`).
attach *filter:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    it_env
    export HOPRD_CLUSTER_DATA_DIR='{{data_dir}}'
    HOPRNET_SHELL='{{hoprnet}}' cargo_it integration '{{filter}}'

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

# CI-equivalent: resolve hoprd/edge-client/blokli `main` (or overrides), build, run the TIER
# (default `pr`) suites, PIX included.
ci:
    nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Kill stray chain/node processes and remove the temp dirs.
clean:
    -bash scripts/integration/lib.sh chain_stop
    -bash scripts/integration/lib.sh reap_nodes
    -bash scripts/integration/lib.sh clean_tmp
