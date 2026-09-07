# hopr-integration-tests

Cross-repo **integration throughput test** for the HOPR stack: stands up a
3-node `hoprd-localcluster` (anvil + blokli + 3 `hoprd` processes, full-mesh
channels) plus a pre-funded edge identity, boots an `edgli` edge client, and
pumps a payload through **0-hop and 1-hop UDP sessions** to the exit node's
built-in loopback — measuring goodput and datagram loss.

It is the gate for the **hoprd v4 line** (`release/4.1`) against `edge-client`
`release/4.1` and blokli `release/0.13`, and runs on a dedicated self-hosted Hetzner
runner (label `hetzner`).

It is wired into all three upstream repos: a hoprd merge to its v4 line **blocks**
on it before the docker image is built or tagged, so a red gate means nothing gets
promoted or deployed; edge-client (`release/4.1`) and blokli (`release/0.13`) fire it on
merge and are told about failures via Zulip; and a PR in any of the three labelled
`run-integration` runs it as a check on that PR. Details and the token/permission
prerequisites are in [`runner/README.md`](runner/README.md).

- Test crate: [`integration/`](integration/)
- CI workflow: [`.github/workflows/integration.yaml`](.github/workflows/integration.yaml)
- Runner: [`runner/README.md`](runner/README.md)

> Legacy k6 load tests live under `k6/` + `echo-service/` and are unrelated to
> the integration test below.

## Framework

Each hop count is its own `#[test]` in `integration/tests/integration.rs`
(`zero_hop`, `one_hop`), reported independently. Every test owns
its cluster: bring up → run → tear down. Three source modules:

| Module       | Responsibility                                                                                              |
| ------------ | ----------------------------------------------------------------------------------------------------------- |
| `cluster.rs` | bring up / attach to `hoprd-localcluster`; contracts are deployed by the chain container (see note below)   |
| `env.rs`     | `IntegrationEnv`: cluster + booted `edgli` + open channels; `open_unreliable_session(hops)` session factory |
| `pump.rs`    | reusable goodput/loss pump; returns a `Transfer` result                                                     |

**Contracts:** the chain deploys the full HOPR contract set on startup (anvil →
`blokli-contract-deployer` → addresses baked into the bloklid config), whether it
comes from the flake-built binary chain (what CI uses — `run.sh` → `run-binchain.sh`
with the latest `blokli` release — and recommended locally) or the `bloklid-anvil`
docker image (a local-only alternative). The framework never deploys contracts. Only
an external `HOPRD_CHAIN_URL` pointed at a foreign chain would lack them.

### Adding a scenario

Add a `#[test_log::test(tokio::test(...))] #[ignore]` fn to
`tests/integration.rs` that calls `run_hop(hops, name)`. Thresholds
(`PAYLOAD_BYTES`, `MIN_ARRIVAL_PCT`, `PUMP_TIMEOUT`) are hardcoded constants —
there is nothing to configure.

### Manual test binaries (not run in CI)

Four extra test binaries reuse the same `IntegrationEnv` harness. CI now runs every
one that a local cluster can drive — `return_path` and `exit_origination` alongside
`integration` — and skips only the two that need something a cluster cannot give:

| Binary                      | What it needs                                             | In CI | Run with                    |
| --------------------------- | --------------------------------------------------------- | ----- | --------------------------- |
| `tests/integration.rs`      | a 3-node cluster                                          | yes   | `just integration-binchain` |
| `tests/return_path.rs`      | a 5-node cluster (more CPU than the throughput tests)     | yes   | `just return-path`          |
| `tests/exit_origination.rs` | a cluster + a pseudonym-lifetime wait                     | yes   | `just exit-origination`     |
| `tests/rotsee.rs`           | a funded Gnosis identity + exit node (`EDGLI_ROTSEE_*`)   | no    | `just rotsee`               |
| `tests/profiling.rs`        | `--features prof` + `--profile tracer` + `tokio_unstable` | no    | `just profile`              |

`rotsee` cannot run in CI (no funded identity) and `profiling` should not: it emits
Perfetto/tokio-console traces rather than a pass/fail verdict, and needs its own build.
Everything else runs on every gate — 5 scenarios, each with a fresh chain. Three
`return_path` scenarios are held out as flaky; see
[`runner/README.md`](runner/README.md).

- **Return path** reproduces the 2026-08-11 return-path break. Sessions are opened with a
  **0-hop forward and 1-hop return** path, so the only packets a cluster node forwards are
  replies — `hopr_packets_count{type="forwarded"}` per node then reads directly as a
  histogram of return-path first relayers (`src/relayers.rs`). One scenario asserts that
  histogram is spread rather than a spike; the other SIGKILLs the busiest return relayer
  and requires the stream to keep flowing. Needs more relayer candidates than the
  throughput tests, so it asks for a 5-node cluster via `cluster::request_cluster_size`
  (`HOPRD_CLUSTER_SIZE` sets the default elsewhere; `hoprd-localcluster` caps at 5).
- **Rotsee** (`IntegrationEnv::setup_rotsee`) boots `edgli` on a pre-funded, on-chain
  identity read from `EDGLI_ROTSEE_*` — no cluster is started — and pumps 0-hop/1-hop
  loopback sessions to a configured exit node. See the header of `tests/rotsee.rs` for the
  env-var contract.
- **Profiling** captures tokio-console + Perfetto traces contrasting a healthy paced pump
  with an executor-starving continuous pump (`pump::pump_continuous`). Driven by
  [`scripts/profile-executor-yield.sh`](scripts/profile-executor-yield.sh); traces land in
  `$EDGLI_TRACE_DIR` (default `./profiling-results`), load them at <https://ui.perfetto.dev>.

For a whole-process CPU flamegraph of the Rotsee path (samply / cargo-flamegraph), see
[`docs/flamegraph.md`](docs/flamegraph.md).

---

## What it measures

| Field (`Transfer`) | Meaning                                                                   |
| ------------------ | ------------------------------------------------------------------------- |
| `mbps`             | Return **goodput** = bytes echoed back / (first→last byte), MB/s (logged) |
| `arrival_pct()`    | `received / sent` — UDP is unreliable, some loss is normal                |
| `sha_ok`           | `true` only on a lossless, byte-identical round-trip                      |

Sessions are **UDP** (HOPR unreliable socket, no retransmission), configured to
mirror `gnosis_vpn-client`'s main (WG) data session — `Segmentation | NoDelay`,
`always_max_out_surbs`, and a production-scale SURB budget (10 MB response
buffer, 16 Mb/s SURB upstream). The exit's SURB egress rate control is left
**on**, so the numbers reflect the real rate-controlled path. Under-provisioning
the SURB budget (mint ceiling below the downlink packet rate) starves the exit's
return path and collapses arrival — the config here matches production so it
does not.

### Gates (per test, hardcoded)

- **Arrival** < `MIN_ARRIVAL_PCT` (99%) → fail (broken/lossy path).
- **Corruption** → full payload returned but bytes differ → fail.

Goodput (`mbps`) is logged but not gated.

---

## Running the test

The chain can come from two places:

- **Binary chain (recommended, no docker):** anvil + bloklid built from the
  blokli flake at its **`release/0.13`** branch (`github:hoprnet/blokli/release/0.13#bloklid`),
  attached via `--chain-url`. Every scenario gets a fresh locally-built chain.
  This is the path CI uses, and `release/0.13` is the line the Jura (v4) network
  runs. It is a **moving branch**, so the build passes `--refresh`, without which
  nix would reuse its cached revision for the branch for up to `tarball-ttl` (1h).
  Note the branch can sit ahead of the exact build Jura deploys (branch head was
  0.13.2 while jura-dev/prod pinned 0.13.1 on 2026-09-03).
- **Docker image (local alternative):** the `bloklid-anvil` image at a **floating**
  tag, which can drift ahead of the pinned `hoprd`/`edgli` and break local runs
  with schema skew. It is also **not** v4-aligned: the registry publishes no jura
  tag and no clean `0.13.x` tag for `bloklid-anvil` (only `0.13.1-commit.*` and
  `-pr.*`). Prefer the binary chain locally; CI does not use this path.

### Quickstart (`just`)

```bash
# recommended: binary chain (blokli from flake branch release/0.13, no docker)
just build-chain             # bloklid + blokli-contract-deployer + anvil, from the blokli flake tag
just integration-binchain    # build hoprd, run both scenarios against a fresh flake chain per scenario
just integration-binchain zero_hop   # one scenario

just unit                # fast unit tests (no cluster)

# docker-image path (LOCAL alternative — CI uses the binary chain; floating :latest tag may drift):
just integration         # build binaries, preflight (pull image), run both tests
just scenario zero_hop   # one test, fresh env
just preflight           # docker + nix + chain-image doctor
just ci                  # CI-equivalent: build from the v4 line / latest (or overrides)
# fast iteration — one cluster, many runs (docker path):
just cluster-up          # terminal 1 (blocks)
just attach one_hop      # terminal 2
just clean               # tear down container + temp state
```

`just --list` shows all recipes. The blokli tag is the `blokli_ref` var in the
justfile — `release/0.13`, a moving branch, so it needs no bumping per patch
release (override: `just blokli_ref=<ref> build-chain`, or `BLOKLI_REF=<ref>`).
The hoprd branch is the `hoprd_ref` var
(default `release/4.1`, the v4 line — override with `just hoprd_ref=<ref> build`
or `HOPRD_REF=<ref>`). Set `HOPRNET_SHELL=path:../hoprnet` to use a
local checkout for the dev shell instead of the flake. The rest of this section
documents the underlying env contract the recipes set up.

The test is `#[ignore]` — it needs external binaries + a container runtime.

### Prerequisites

| Var                       | Required      | Meaning                                                                                                        |
| ------------------------- | ------------- | -------------------------------------------------------------------------------------------------------------- |
| `HOPRD_BIN`               | managed mode  | path to a `hoprd` binary                                                                                       |
| `HOPRD_LOCALCLUSTER_BIN`  | always        | path to a `hoprd-localcluster` binary                                                                          |
| `HOPRD_CHAIN_IMAGE`       | managed mode  | a `bloklid-anvil` image tag                                                                                    |
| `HOPRD_CONTAINER_RUNTIME` | no            | `docker` (default), `container`, `podman`                                                                      |
| `HOPRD_CLUSTER_DATA_DIR`  | external mode | data-dir of an already-running cluster                                                                         |
| `HOPRD_CHAIN_URL`         | binary chain  | attach to an external blokli (e.g. `http://localhost:8080`); skips the container, replaces `HOPRD_CHAIN_IMAGE` |

Docker is the only external service: the chain (anvil + blokli + contracts) runs
as a single `bloklid-anvil` container on the host daemon — `localcluster` launches
it with `docker run --platform linux/amd64 -p 8080:8080 …` (auto-pulls if absent)
and removes it on exit. No docker-in-docker. The host just needs the docker daemon
up and registry auth. [`scripts/integration/preflight.sh`](scripts/integration/preflight.sh)
checks both and pulls the image (idempotent — also a local "doctor"):

```bash
scripts/integration/preflight.sh <bloklid-anvil-image-ref>
```

Build the binaries from the [`hoprnet/hoprd`](https://github.com/hoprnet/hoprd) repo
(pass a ref to test a branch/PR, e.g. `github:hoprnet/hoprd/<sha>#…`):

```bash
nix build -L github:hoprnet/hoprd#binary-hoprd-x86_64-linux              --out-link result-hoprd
nix build -L github:hoprnet/hoprd#binary-hoprd-localcluster-x86_64-linux --out-link result-localcluster
# on macOS use the bare names: .#binary-hoprd and .#binary-hoprd-localcluster
```

For the chain, prefer the flake binary chain over the docker image — build blokli
(anvil + bloklid) from its **`release/0.13`** branch (Cachix-cached):

```bash
nix build -L --refresh 'github:hoprnet/blokli/release/0.13#bloklid' --out-link result-bloklid   # --refresh: the branch moves
nix build -L 'nixpkgs#foundry'                                      --out-link result-foundry   # anvil
```

Only if you must use the docker path instead: `docker pull
europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest`
(floating tag — may drift ahead of the pinned binaries).

### Managed mode (test owns the cluster lifetime)

```bash
export HOPRD_BIN=$PWD/result-hoprd/bin/hoprd
export HOPRD_LOCALCLUSTER_BIN=$PWD/result-localcluster/bin/hoprd-localcluster
export HOPRD_CHAIN_IMAGE=europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest
export RUST_LOG=info,edgli=debug

cd integration
cargo test --test integration -- --include-ignored --test-threads=1   # both tests
# one hop count: append `zero_hop` or `one_hop` before the `--`
```

The cluster + chain container are torn down automatically on exit.

### Binary-chain mode (recommended — flake blokli, no docker)

Set `HOPRD_CHAIN_URL` instead of `HOPRD_CHAIN_IMAGE`; localcluster then attaches to
an already-running blokli rather than starting a container.
[`scripts/integration/run-binchain.sh`](scripts/integration/run-binchain.sh) wires
this up — it starts a fresh flake-built chain per scenario and tears it down:

```bash
export HOPRD_BIN=$PWD/result-hoprd/bin/hoprd                     # from a PR? build that ref
export HOPRD_LOCALCLUSTER_BIN=$PWD/result-localcluster/bin/hoprd-localcluster
SCENARIOS="zero_hop one_hop" bash scripts/integration/run-binchain.sh
```

`result-bloklid` / `result-foundry` must exist (`just build-chain`, or the two
`nix build` commands above). This is the path `just integration-binchain` drives.

### External mode (attach to a running cluster — faster iteration)

```bash
# terminal 1: bring the cluster up once and leave it running
hoprd-localcluster --size 3 --extra-identities 1 \
  --api-port-base 13000 --p2p-port-base 19000 \
  --api-token test-token-localcluster \
  --chain-image $HOPRD_CHAIN_IMAGE \
  --hoprd-bin $HOPRD_BIN \
  --data-dir /tmp/hopr-it

# terminal 2: once `hoprd-localcluster status --data-dir /tmp/hopr-it` reports
# "state": "running", run the test repeatedly without re-bringup
export HOPRD_LOCALCLUSTER_BIN=$PWD/result-localcluster/bin/hoprd-localcluster
export HOPRD_CLUSTER_DATA_DIR=/tmp/hopr-it
cd integration && cargo test --test integration -- --include-ignored --test-threads=1
```

There are no tuning knobs — payload size, arrival floor, and timeout are
hardcoded constants in `tests/integration.rs`.

Unit tests (cluster status parsing, no external deps): `cargo test --lib`.

---

## CI

`pr.yaml` runs on every PR, in three jobs split by what each one needs: the PR
title check (Conventional Commits) and `lint` (`cargo fmt --check` +
`cargo clippy -D warnings`) on hosted **depot** runners, and `unit`
(`cargo test --lib`) on the self-hosted **`hetzner`** box — anything that
_executes_ a test runs on the same machine as the throughput gate, so results are
comparable. The `#[ignore]` e2e is **not** run here. All three build in the
hoprnet dev shell. Locally: `just lint` + `just unit`.

`integration.yaml` runs on `repository_dispatch[integration]` (fired by `hoprd` /
`edge-client` on merge), on manual `workflow_dispatch`, and on a hopr-integration-tests PR
labelled **`run-integration`** (to test changes to this repo against the live
stack). Concurrency: a new push to a PR **cancels** that PR's in-progress run;
dispatch/manual runs **stack** (shared group, never cancelled) and execute one
after another. **No version state is stored:** the triggering project supplies its
rev via the dispatch; `hoprd` otherwise defaults to the **v4 line** and
`edge-client` to its `release/4.1` HEAD; **blokli always tracks the head of its
`release/0.13` branch** — the Jura (v4) line — re-resolved per run
(`nix build --refresh`) and built from its flake. So every run tests one project's
change against the current tip of the other and the current 0.13 blokli. `run.sh` builds `hoprd` + `hoprd-localcluster` from the hoprd ref, builds
the blokli chain from the release, pins `edgli` to the resolved edge-client sha,
runs the tests against a fresh flake chain per scenario (`run-binchain.sh`), and
notifies Zulip on red — naming the trigger and the resolved hoprd/edge-client/blokli
versions, not the dispatch inputs (see [`runner/README.md`](runner/README.md)).
Nothing is committed back.

### hoprd v4 / v5

hoprd `main` is **v5**. This test targets **v4**: the integration crate pins
`hopr-lib` to hoprnet `release/4.0`, which is what `edge-client` `release/4.1`
resolves too — its `main` repinned to hoprnet `master` (v5) in #151 on 2026-09-04,
which is why the v4 branch exists. A v5 hoprd binary would run against a v4
library set. `run.sh`
therefore builds hoprd from `HOPRD_LINE` — **`release/4.1`**, hoprd's only v4
branch — and rejects a dispatched hoprd rev that is not contained in it (bypass:
`HOPRD_SKIP_LINE_CHECK=1`). hoprd's merge workflow should only dispatch from
`release/4.1`.

Defaults are overridable via repo variables `HOPRD_LINE`, `HOPRD_REF`,
`EDGLI_REF`, `BLOKLI_REF` (unset → `release/4.1` / same / `main` /
`release/0.13`).

Manual run:

```bash
gh workflow run integration.yaml -R hoprnet/hopr-integration-tests \
  -f project=hoprd -f rev=<sha>          # or project=edge-client
# empty inputs → hoprd at release/4.1, edge-client at main, blokli at release/0.13
```

Runs on the self-hosted **`hetzner`** runner, provisioned from the gitops repo
(`ansible/playbooks/install-github-hetzner-runner.yaml`). Nix and the `hoprnet`
Cachix substituter must be present **on the box** — the `setup-nix` action skips
both once nix is on PATH. See [`runner/README.md`](runner/README.md) for the
prerequisites, the shared-box concurrency caveat, the repo secrets, and validation
steps.
