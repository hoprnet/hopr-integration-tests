# hopr-integration-tests

Cross-repo **integration throughput test** for the HOPR stack: stands up a
3-node `hoprd-localcluster` (anvil + blokli + 3 `hoprd` processes, full-mesh
channels) plus a pre-funded edge identity, boots an `edgli` edge client, and
pumps a payload through **0-hop and 1-hop UDP sessions** to the exit node's
built-in loopback — measuring goodput and datagram loss.

It gates **both release lines** from one crate — the test bodies are shared source and
only the dependency set differs — and runs on a dedicated self-hosted Hetzner runner
(label `hetzner`):

|                  | hoprd         | hoprnet       | edge-client   | blokli         | PIX suite |
| ---------------- | ------------- | ------------- | ------------- | -------------- | --------- |
| **v4** (default) | `release/4.1` | `release/4.0` | `release/4.1` | `release/0.13` | no        |
| **v5**           | `main`        | `master`      | `main`        | `v0.14.0`      | yes       |

Pick one with `LINE=v4`/`LINE=v5` (`just ci` / `just ci-v5`, or the `line` input on
`integration.yaml`). The two are not mixable: a v4 blokli cannot bootstrap a v5
localcluster (`service_registry` first appears in v0.14.0), and edge-client `main`
repinned hopr-lib to hoprnet `master` in #151, so a v5 edge client pairs with a v4 hoprd
only by accident. PIX exists on v5 alone — its deposit pool (`edgli/pix-test`) has no v4
counterpart.

**A nightly run at 02:00 UTC is the automatic coverage for the v4 lines**, testing the
current tips of all three together. It has to be a schedule rather than a merge gate: a
merge queue can only be attached to a repository's default branch, and all three develop
v4 on `release/*` branches, so none of them can gate merges into those branches.

hoprd, edge-client and blokli each also carry a `run-integration` label that runs this
suite against a PR on demand. Their `merge_group` condition is there for the day the
default-branch restriction lifts; it cannot fire for a release branch today.

Details and the token/permission prerequisites are in
[`runner/README.md`](runner/README.md).

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

**Contracts:** `scripts/integration/lib.sh chain_up` deploys the full HOPR contract set on
startup (anvil → `blokli-contract-deployer` → addresses baked into the bloklid config), so by the
time the chain answers GraphQL the nodes are served live addresses. Neither the framework nor
localcluster deploys contracts. Only an `HOPRD_CHAIN_URL` pointed at a foreign chain would lack
them.

### Adding a scenario

Add a `#[test_log::test(tokio::test(...))] #[ignore]` fn to
`tests/integration.rs` that calls `run_hop(hops, name)`. Thresholds
(`PAYLOAD_BYTES`, `MIN_ARRIVAL_PCT`, `PUMP_TIMEOUT`) are hardcoded constants —
there is nothing to configure.

### Manual test binaries (not run in CI)

Five extra test binaries reuse the same `IntegrationEnv` harness. CI runs the ones a
local cluster can drive and whose verdict is trustworthy — `integration`,
`exit_origination`, and `pix` on v5:

| Binary                      | What it needs                                                               | In CI   | Run with                         |
| --------------------------- | --------------------------------------------------------------------------- | ------- | -------------------------------- |
| `tests/integration.rs`      | a 3-node cluster                                                            | yes     | `just integration-binchain`      |
| `tests/return_path.rs`      | a 5-node cluster (more CPU than the throughput tests)                       | no      | `just return-path`               |
| `tests/exit_origination.rs` | a cluster + a pseudonym-lifetime wait                                       | yes     | `just exit-origination`          |
| `tests/pix.rs`              | a PIX-enabled `hoprd` (flake output on linux, source build on darwin)       | v5 only | `LINE=v5 just pix`               |
| `tests/pix_shapes.rs`       | the above, plus a `hoprd-localcluster` taking `--pix-config`                | v5 only | `LINE=v5 just pix-shapes`        |
| `tests/upload_survival.rs`  | a cluster; fails until the reply-opener fix (hoprnet#8417) reaches the line | no      | see `scripts/integration/run.sh` |
| `tests/rotsee.rs`           | a funded Gnosis identity + exit node (`EDGLI_ROTSEE_*`)                     | no      | `just rotsee`                    |
| `tests/profiling.rs`        | `--features prof` + `--profile tracer` + `tokio_unstable`                   | no      | `just profile`                   |

`rotsee` cannot run in CI (no funded identity) and `profiling` should not: it emits
Perfetto/tokio-console traces rather than a pass/fail verdict, and needs its own build.
`return_path` is held out for a different reason — every one of its scenarios asserts an
arrival ratio over an unforced random relayer draw, so a red there says nothing; see
[`runner/README.md`](runner/README.md). What remains runs on every gate: 3 scenarios on
v4, 11 on v5, each with a fresh chain.

- **Return path** reproduces the 2026-08-11 return-path break. Sessions are opened with a
  **0-hop forward and 1-hop return** path, so the only packets a cluster node forwards are
  replies — `hopr_packets_count{type="forwarded"}` per node then reads directly as a
  histogram of return-path first relayers (`src/relayers.rs`). One scenario asserts that
  histogram is spread rather than a spike; the other SIGKILLs the busiest return relayer
  and requires the stream to keep flowing. Needs more relayer candidates than the
  throughput tests, so it asks for a 5-node cluster via `cluster::request_cluster_size`
  (`HOPRD_CLUSTER_SIZE` sets the default elsewhere; `hoprd-localcluster` caps at 5).
- **PIX** runs the settlement protocol end to end with **`edgli` as the paying Entry**, which is
  the configuration that ships (`gnosis_vpn-client` embeds `edgli`) and which hoprd's own
  `session_pix` does not cover — that one is hoprd-Entry ↔ hoprd-Exit over the REST API.
  Deposits are debited from the entry's **Safe** (hopr-types 4.0.0 routes the transfer through the
  Safe module), which is also where the channel stakes live — so a run is bounded by the
  strategy's `max_spend_per_window` rather than by an exact float, and the entry's side is counted
  off `hopr_strategy_pix_*` rather than divided out of a balance two strategies spend.
  One scenario asserts the money reconciles: the Exit's Safe gains an exact whole multiple of
  `price_per_byte × quota`, the entry reports the same count in deposits, and the entry's Safe fell
  by at least what the Exit's gained. The other pins the documented failure mode — an entry that
  reaches its deposit budget stops paying, the Exit's deposit deadline fires, and it sweeps only the
  cycles it was paid for. Measured, the entry gets **no event at all** when that happens: an
  unreliable session carries no end-of-stream, so the closure arrives as replies ceasing. An
  embedder that wants to react has to watch its own counters and Safe balance
  (`IntegrationEnv::entry_safe_balance`), not the session. Both need a `hoprd` carrying a deposit
  pool, which is a non-default cargo feature the nix
  flake does not build, so `just pix` compiles it from `HOPRD_SRC` (default `../hoprd`) and checks
  the resulting binary for its pool marker before starting a cluster. See
  [`integration/tests/pix.rs`](integration/tests/pix.rs) for the pacing constants, which are
  load-bearing.
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

The chain is anvil + bloklid built from the **blokli flake at its latest release**
(`github:hoprnet/blokli/<tag>#bloklid`, currently `v0.14.0`, the first with the
`service_registry` contract address the current `hoprd-localcluster` requires), started by
`scripts/integration/lib.sh chain_up` and attached via `--chain-url`. It pins a concrete blokli
release rather than a floating docker tag.

### Quickstart (`just`)

```bash
just build-chain             # bloklid + blokli-contract-deployer + anvil, from the blokli flake tag
just integration-binchain    # build hoprd, run both scenarios on one chain + cluster
just integration-binchain zero_hop   # one scenario

just unit                # fast unit tests (no cluster)
just ci                  # CI-equivalent: the whole v4 line (or overrides)
just ci-v5               # same on the v5 line, PIX suite included

# fast iteration — one cluster, many runs:
just cluster-up          # terminal 1 (blocks)
just attach one_hop      # terminal 2
just clean               # kill stray processes + temp state
```

`just --list` shows all recipes. The refs come from the `line` var (default `v4`, set with
`LINE=v5`), which derives `blokli_ref` and `hoprd_ref`; override either individually with
`just blokli_ref=<ref> build-chain` / `just hoprd_ref=<ref> build`, or `BLOKLI_REF=` /
`HOPRD_REF=`. `release/0.13` is a moving branch, so the v4 blokli needs no bumping per
patch release. Set `HOPRNET_SHELL=path:../hoprnet` to use a
local checkout for the dev shell instead of the flake. The rest of this section
documents the underlying env contract the recipes set up.

The test is `#[ignore]` — it needs external binaries and a running chain (`lib.sh chain_up`;
see Managed mode below).

### Seeing the logs

```bash
RUST_LOG=info,edgli=debug TEST_ARGS=--nocapture HOPRD_KEEP_ARTIFACTS=1 \
  just integration-binchain zero_hop
```

| Where | How |
| ----- | --- |
| Per-node `hoprd_<i>.log` | `HOPRD_KEEP_ARTIFACTS=1`. The cluster dir is a `TempDir` dropped at teardown, so the evidence is gone by the time a failure is worth reading. The run logs the path it keeps (`/tmp/hoprd-it-*/…/hoprd_<i>.log`, or under `/tmp/nix-shell.*/` in the dev shell). Leaks on purpose — `rm -rf /tmp/hoprd-it-*` afterwards. |
| Harness + stack tracing | `RUST_LOG`, default `info,edgli=debug`. That narrates every packet — ~6900 lines to surface ~44. CI uses `warn,hoprd_integration_test=info`; start there and widen. |
| anvil / bloklid / deployer | Always written to `/tmp/hopr-chain/*.log`. Tailed to stderr automatically if the chain dies during startup. |
| nix build output | On the console for `just`; `run.sh` redirects it to `nix-build.log`, which is why a long build there looks like a hang. |

### Prerequisites

| Var                       | Required      | Meaning                                                                                                                                     |
| ------------------------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `HOPRD_BIN`               | managed mode  | path to a `hoprd` binary                                                                                                                    |
| `HOPRD_LOCALCLUSTER_BIN`  | always        | path to a `hoprd-localcluster` binary                                                                                                       |
| `HOPRD_CLUSTER_DATA_DIR`  | external mode | data-dir of an already-running cluster                                                                                                      |
| `HOPRD_CHAIN_URL`         | managed mode  | the blokli to attach to (default `http://localhost:8080`), started by `lib.sh chain_up`                                                     |
| `HOPRD_SRC`               | `just pix`    | hoprd checkout to build the PIX binaries from (default `../hoprd`); built from source since the flake exposes no binary with a deposit pool |

No container runtime is needed: the chain is three local binaries
(`scripts/integration/lib.sh chain_up` runs anvil → `blokli-contract-deployer` → bloklid) and
localcluster attaches to it over `--chain-url`.

Build the binaries from the [`hoprnet/hoprd`](https://github.com/hoprnet/hoprd) repo
(pass a ref to test a branch/PR, e.g. `github:hoprnet/hoprd/<sha>#…`):

```bash
nix build -L github:hoprnet/hoprd#binary-hoprd              --out-link result-hoprd
nix build -L github:hoprnet/hoprd#binary-hoprd-localcluster --out-link result-localcluster
# these are what `run.sh` builds. The flake also names per-system outputs
# (`-x86_64-linux`, …), but only `binary-hoprd` has one for every system —
# `binary-hoprd-localcluster` is x86_64-linux only. Cross-build with NIX_SYSTEM_SUFFIX.
```

For the chain, build blokli (anvil + bloklid) from its release tag (Cachix-cached):

```bash
nix build -L 'github:hoprnet/blokli/v0.14.0#bloklid' --out-link result-bloklid   # a blokli release (CI resolves the latest per run)
nix build -L 'nixpkgs#foundry'                       --out-link result-foundry   # anvil
```

### Managed mode (the runner owns the chain and cluster)

[`scripts/integration/run-binchain.sh`](scripts/integration/run-binchain.sh) starts one fresh
chain and one cluster per test binary, reused by every scenario in it, and tears both down:

```bash
export HOPRD_BIN=$PWD/result-hoprd/bin/hoprd                     # from a PR? build that ref
export HOPRD_LOCALCLUSTER_BIN=$PWD/result-localcluster/bin/hoprd-localcluster
SCENARIOS="zero_hop one_hop" bash scripts/integration/run-binchain.sh
```

`result-bloklid` / `result-foundry` must exist (`just build-chain`, or the two
`nix build` commands above). This is the path `just integration-binchain` drives.

### External mode (attach to a running cluster — faster iteration)

```bash
# terminal 1: bring chain + cluster up once and leave them running (`just cluster-up`)
bash scripts/integration/lib.sh chain_up &
hoprd-localcluster --size 3 --extra-identities 1 \
  --api-host 127.0.0.1 \
  --api-port-base 13000 --p2p-port-base 19000 \
  --api-token test-token-localcluster \
  --chain-url http://localhost:8080 \
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

`pr.yaml` runs on every PR, in four jobs split by what each one needs: the PR
title check (Conventional Commits), `lint` (`cargo fmt --check` +
`cargo clippy -D warnings`) and `shell` (shellcheck + shfmt, plus the script-count
budget) on hosted **depot** runners, and `unit` (`cargo test --lib`) on the
self-hosted **`hetzner`** box — anything that _executes_ a test runs on the same
machine as the throughput gate, so results are comparable. The `#[ignore]` e2e is
**not** run here. Locally: `just lint` + `just unit`.

Both fan out over the two lines (`v4`, `v5`), which is where `--features pix` gets checked
at all: the v5 job adds a `--features pix` clippy pass and runs `cargo test --lib
--features pix`, so the PIX-only parts of `src/pix.rs` are compiled and linted rather than
skipped. On v4 the `pix` feature is declared but inert — enabling it there does not compile,
because `edgli/pix-test` does not exist on that line.

`integration.yaml` runs on `repository_dispatch[integration]` (fired by `hoprd` /
`edge-client` on merge), on manual `workflow_dispatch`, and on a hopr-integration-tests PR
labelled **`run-integration`** (to test changes to this repo against the live
stack). A dispatch picks its line (`line` input / `client_payload.line`, default `v4`);
the triggers that gate **this** repo — its merge queue and a labelled PR — fan out over
**both** lines, so a change to the shared test bodies has to hold on either dependency
set. The job name carries the line, so required checks must name
`Integration throughput (v4)` and `Integration throughput (v5)`. Concurrency: a new push to a PR **cancels** that PR's in-progress run;
dispatch/manual runs **stack** (shared group, never cancelled) and execute one
after another. **No version state is stored:** the triggering project supplies its
rev via the dispatch and every other ref defaults to the head of its branch on the
selected line, re-resolved per run (`nix build --refresh` — nix otherwise caches a
branch's resolved revision for an hour, which would silently rebuild the previous one).
So every run tests one project's change against the current tip of the other two. `run.sh` builds `hoprd` + `hoprd-localcluster` from the hoprd ref, builds
the blokli chain from the release, pins `edgli` to the resolved edge-client sha,
runs the tests against a fresh flake chain per scenario (`run-binchain.sh`), and
notifies Zulip on red — naming the trigger and the resolved hoprd/edge-client/blokli
versions, not the dispatch inputs (see [`runner/README.md`](runner/README.md)).
Nothing is committed back.

### Selecting a line

`LINE` (default `v4`) picks every ref in the table at the top of this file, and with it
the dependency set: `integration/Cargo.toml` is the v4 set, `integration/Cargo.v5.toml`
the v5 one. `run.sh` copies the v5 file over `Cargo.toml` for the run and restores it on
exit — Cargo insists the manifest be named `Cargo.toml`, so it cannot simply be selected
with `--manifest-path`. After pinning `edgli` to the resolved sha it mirrors
**edge-client's own** `hopr-lib` pin onto ours (v5 has a direct dep, v4 has none) and then
fails the run if the lock ends up with two copies of `hopr-lib` or `hopr-strategy` — that
split registers each metric in one copy and increments it from the other, so every
reading comes back zero instead of erroring.

`HOPRD_LINE` is the branch the hoprd binaries come from **and** the line a dispatched
hoprd rev must be contained in; a rev from the other side of the split is rejected before
the build rather than after it (bypass: `HOPRD_SKIP_LINE_CHECK=1`). Each upstream repo
should dispatch only from the branch matching the line it asks for.

Per-project overrides still win, via repo variables `HOPRD_LINE`, `HOPRD_REF`,
`EDGLI_REF`, `BLOKLI_REF` — all unset by default so each one follows `LINE`.

Manual run:

```bash
gh workflow run integration.yaml -R hoprnet/hopr-integration-tests \
  -f line=v5 -f project=hoprd -f rev=<sha>   # or project=edge-client / blokli
# empty inputs → the v4 line, every ref at its branch head
```

Runs on the self-hosted **`hetzner`** runner, provisioned from the gitops repo
(`ansible/playbooks/install-github-hetzner-runner.yaml`). Nix and the `hoprnet`
Cachix substituter must be present **on the box** — the `setup-nix` action skips
both once nix is on PATH. See [`runner/README.md`](runner/README.md) for the
prerequisites, the shared-box concurrency caveat, the repo secrets, and validation
steps.
