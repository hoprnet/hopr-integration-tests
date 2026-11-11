# Integration throughput test — convenience recipes.
#
# Local quickstart (recommended — binary chain, blokli from flake branch release/0.13, no docker):
#   just build-chain            # build blokli(anvil+bloklid) from the flake release
#   just integration-binchain   # build hoprd, run all scenarios against a fresh flake chain
#   just unit                   # fast unit tests (no cluster)
#
# Fast iteration (one cluster, many runs):
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

# Release line every ref below derives from: v4 (default) or v5. Mirrors LINE in
# scripts/integration/run.sh, whose header carries the branch table. The lines are not mixable.
line := env_var_or_default("LINE", "v4")

# Blokli branch for the image-free binary chain. v5 needs 0.14+: its contract addresses carry
# `service_registry`, which the v5 chain API requires; against v0.13.0 or earlier
# `hoprd-localcluster` exits during bootstrap with "contract addresses not a valid JSON: missing
# field `service_registry`". Override: `just blokli_ref=… build-chain`, or set BLOKLI_REF.
blokli_ref := env_var_or_default("BLOKLI_REF", if line == "v5" { "main" } else { "release/0.13" })

# hoprd branch the binaries are built from (override: `just hoprd_ref=… build`, or HOPRD_REF).
hoprd_ref := env_var_or_default("HOPRD_REF", if line == "v5" { "main" } else { "release/4.1" })

# hoprd checkout to build the PIX binaries (`just pix`) and the TRACE binaries
# (`just integration-trace`) from. hoprd's flake does expose
# `binary-hoprd-pix-test`, but for x86_64-linux only — which is what CI uses and what a darwin
# workstation cannot build — so `just pix` compiles hoprd from this tree instead.
hoprd_src := env_var_or_default("HOPRD_SRC", "../hoprd")

# LINE=v5 runs v5 binaries, so the crate needs the v5 manifest too (run.sh does its own swap).
v5_deps := if line == "v5" { "bash scripts/integration/with-v5-deps.sh" } else { "" }

data_dir := "/tmp/hopr-it"

_default:
    @just --list

# Build local-arch hoprd + hoprd-localcluster binaries from the selected line's hoprd branch
# (nix, Cachix-cached). CI builds the same branch, or the rev from a merge
# dispatch (see `just ci` / scripts/integration/run.sh).
build:
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd' --out-link result-hoprd
    nix build -L 'github:hoprnet/hoprd/{{hoprd_ref}}#binary-hoprd-localcluster' --out-link result-localcluster

# Build the image-free chain: bloklid + blokli-contract-deployer (blokli branch)
# and anvil (nixpkgs foundry). Replaces the bloklid-anvil docker image. `--refresh`
# so a moved branch head is picked up instead of nix's cached revision for it.
build-chain:
    nix build -L --refresh 'github:hoprnet/blokli/{{blokli_ref}}#bloklid' --out-link result-bloklid
    nix build -L 'nixpkgs#foundry' --out-link result-foundry

# Build hoprd + the binary chain, then run the scenarios on one locally-built anvil+bloklid
# and one cluster (via --chain-url). Optional args = scenarios, e.g.
# `just integration-binchain zero_one_two_hop`.
integration-binchain *scenarios: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    # run-binchain.sh enters the dev shell itself, so no outer wrap.
    [ -n '{{scenarios}}' ] && export SCENARIOS='{{scenarios}}'
    HOPRNET_SHELL='{{hoprnet}}' {{v5_deps}} bash scripts/integration/run-binchain.sh

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
    HOPRNET_SHELL='{{hoprnet}}' {{v5_deps}} bash scripts/integration/run-binchain.sh

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
      SCENARIOS="${scenario}" {{v5_deps}} bash scripts/integration/run-binchain.sh || rc=1
    done
    exit "${rc}"

# Exit-origination repro (binary chain): does the exit keep originating packets when
# one of its return paths can never be resolved? See integration/tests/exit_origination.rs.
exit-origination: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    export TEST_TARGET=exit_origination
    HOPRNET_SHELL='{{hoprnet}}' {{v5_deps}} bash scripts/integration/run-binchain.sh

# A relay that acknowledges everything and delivers nothing toward the exit (GNO-1183): does a
# 1-hop session avoid it? See integration/tests/dead_last_hop.rs. Also runs in the v5 `pr` tier of CI. v5 only.
# `EDGLI_REF=<branch|sha>` runs an unmerged edge-client; `NETWORK_GRAPH_REF=<branch>` also patches
# an unreleased hopr-network-graph from hoprnet/hopr-impls (Cargo ignores edge-client's own patch).
# Both need `gh` logged in, to resolve the ref and read edge-client's manifest.
dead-last-hop: build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "run: LINE=v5 just dead-last-hop" >&2; exit 2; }
    # Inherited, it would replace TEST_TARGET in run-binchain.sh and run another suite.
    unset TEST_TARGETS
    export TEST_TARGET=dead_last_hop
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/with-v5-deps.sh bash -c '
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

# End-to-end PIX with edgli as the paying entry (binary chain; manual, NOT run in CI).
# Builds hoprd from HOPRD_SRC (default ../hoprd) because the nix flake has no PIX binary.
# See integration/tests/pix.rs. Optional args = test-name filters.
pix *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "PIX is v5-only — run: LINE=v5 just pix" >&2; exit 2; }
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    export SCENARIOS='{{scenarios}}' TEST_TARGET=pix
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    HOPRNET_SHELL='{{hoprnet}}' bash scripts/integration/with-v5-deps.sh \
      bash scripts/integration/run-binchain.sh

# PIX under end-user traffic shapes (binary chain; manual, NOT run in CI, hours per full pass).
#
# Same build path as `just pix`, but at a geometry whose cycle is long enough for a traffic shape
# to exist inside it — see integration/src/shapes.rs. Needs a HOPRD_SRC carrying the Exit-side PIX
# fill (hoprnet#8396); the idle scenario measures exactly that, and against a hoprd without it an
# idle cycle strands its deposit by design.
#
# Optional args = test-name filters.
pix-shapes *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "PIX is v5-only — run: LINE=v5 just pix-shapes" >&2; exit 2; }
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"
    pix_check_localcluster "${HOPRD_LOCALCLUSTER_BIN}"

    export TEST_TARGET=pix_shapes HOPRNET_SHELL='{{hoprnet}}'
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    runner=(bash scripts/integration/with-v5-deps.sh bash scripts/integration/run-binchain.sh)

    SCENARIOS='{{scenarios}}' "${runner[@]}"

# Run a CI tier locally: the same suites as `run.sh` (lib.sh `run_tier`), from the binaries built
# here rather than resolved refs. v5 also builds the PIX hoprd + localcluster from HOPRD_SRC, a v5
# hoprd git checkout, since the flake's PIX binary is x86_64-linux only. See docs/v5-test-matrix.md.
#   LINE=v5 HOPRD_SRC=../hoprd-main just tier pr
#   LINE=v5 HOPRD_SRC=../hoprd-main just tier nightly
tier name="pr": build build-chain
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/integration/lib.sh
    it_env
    export LINE='{{line}}' TIER='{{name}}' HOPRNET_SHELL='{{hoprnet}}' PIX_BIN=""
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    if [ "${LINE}" = v5 ]; then
      nodes_bin="${HOPRD_BIN}"
      pix_build '{{hoprd_src}}'
      pix_check_hoprd "${HOPRD_BIN}"
      PIX_BIN="${HOPRD_BIN}" HOPRD_BIN="${nodes_bin}"
    fi
    chain_start
    trap chain_stop EXIT
    # v5 runs against edge-client's line head, like CI: the committed v5 lock trails `main`.
    {{v5_deps}} bash -c 'source scripts/integration/lib.sh; if [ "${LINE}" = v5 ]; then pin_line_deps; fi; run_tier'

# PIX when half the return relays die mid-cycle (binary chain; manual, NOT run in CI, about ten
# minutes per scenario at the default geometry). See integration/tests/pix_relay_loss.rs and
# docs/pix-relay-loss.md. Same build path as `just pix`.
#
# One cluster PER SCENARIO, like `return-path` and for the same reason: each kills a cluster node.
# They share one chain, started once. `HOPRD_PIX_*` (surplus, geometry, SURB buffer, rate) are
# forwarded to the test; docs/pix-relay-loss.md lists them. Optional args = test-name filters.
pix-relay-loss *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "PIX is v5-only — run: LINE=v5 just pix-relay-loss" >&2; exit 2; }
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    it_env
    export TEST_TARGET=pix_relay_loss HOPRNET_SHELL='{{hoprnet}}'
    # A failed PIX run is unreadable without the node logs, and they are deleted at teardown.
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    scenarios='{{scenarios}}'
    # Listed under the v5 dependency set: the target is `#![cfg(feature = "v5")]` and lists nothing
    # without it.
    [ -n "${scenarios}" ] || scenarios="$(bash scripts/integration/with-v5-deps.sh \
      bash -c 'source scripts/integration/lib.sh; list_scenarios pix_relay_loss')"
    chain_start
    trap chain_stop EXIT
    export CHAIN_SHARED=1
    rc=0
    for scenario in ${scenarios}; do
      SCENARIOS="${scenario}" bash scripts/integration/with-v5-deps.sh \
        bash scripts/integration/run-binchain.sh || rc=1
    done
    exit "${rc}"

# PIX at the DEPLOYED geometry.
#
# Same build path as `just pix`, but at the geometry hoprnet#8519 ships as its default —
# `4096 x (32 + 79)`, a cycle of 454 656 packets and a 629.6 MiB quota — rather than the demo
# geometry `just pix` uses. See integration/src/prod_pix.rs for why running the real dimensions is
# a different experiment from a scaled profile that agrees with them on every ratio.
#
# One invocation PER SCENARIO, unlike `just pix`: the suite is for scenarios that disturb the
# cluster (a relayer is killed mid-run), so a shared cluster would hand the next one a corpse.
pix-prod *scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    [ '{{line}}' = v5 ] || { echo "PIX is v5-only — run: LINE=v5 just pix-prod" >&2; exit 2; }
    source scripts/integration/lib.sh
    pix_build '{{hoprd_src}}'
    just build-chain
    pix_check_hoprd "${HOPRD_BIN}"

    export TEST_TARGET=pix_prod HOPRNET_SHELL='{{hoprnet}}'
    # Release, not debug, whatever TEST_PROFILE says: the Entry is linked into this binary and
    # generating 4096 polynomials per cycle is CPU-bound enough that a debug build cannot keep up
    # with the Exit's commitment deadline — measured, it stretched the SSA commitment past
    # max_ssa_delivery_time and every cycle failed.
    export TEST_PROFILE=release
    export HOPRD_KEEP_ARTIFACTS="${HOPRD_KEEP_ARTIFACTS:-1}"
    scenarios='{{scenarios}}'
    # Listed under the v5 dependency set: the target is `#![cfg(feature = "v5")]` and lists nothing
    # without it.
    [ -n "${scenarios}" ] || scenarios="$(bash scripts/integration/with-v5-deps.sh \
      bash -c 'source scripts/integration/lib.sh; list_scenarios pix_prod')"
    runner=(bash scripts/integration/with-v5-deps.sh bash scripts/integration/run-binchain.sh)

    rc=0
    for scenario in ${scenarios}; do
      SCENARIOS="${scenario}" "${runner[@]}" || rc=1
    done
    exit "${rc}"

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

# CI-equivalent: resolve every ref on the v4 line (or overrides), build, run every suite.
ci:
    nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Same, on the v5 line: hoprd/edge-client `main`, blokli `main`, PIX suite included.
# run.sh swaps `integration/Cargo.v5.toml` in for the run and restores it on exit.
ci-v5:
    LINE=v5 nix develop {{hoprnet}} -c bash scripts/integration/run.sh

# Kill stray chain/node processes and remove the temp dirs.
clean:
    -bash scripts/integration/lib.sh chain_stop
    -bash scripts/integration/lib.sh reap_nodes
    -bash scripts/integration/lib.sh clean_tmp
