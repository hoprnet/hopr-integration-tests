# hopr-integration-tests

End-to-end tests for the HOPR stack. Each run brings up a local chain (anvil + bloklid), a
`hoprd-localcluster` and an `edgli` edge client, then pushes traffic through HOPR sessions and
checks what comes back.

One crate covers two release lines. The test code is shared, and only the dependency set
differs (`integration/Cargo.toml` = v4, `integration/Cargo.v5.toml` = v5):

|                  | hoprd         | hoprnet       | edge-client   | blokli         | PIX |
| ---------------- | ------------- | ------------- | ------------- | -------------- | --- |
| **v4** (default) | `release/4.1` | `release/4.0` | `release/4.1` | `release/0.13` | no  |
| **v5**           | `main`        | `master`      | `main`        | `release/0.14` | yes |

The lines do not mix. A v0.13 blokli cannot bootstrap a v5 localcluster (no `service_registry`),
and the edge-client lines pin different hopr-lib versions.

Runner setup, secrets and the upstream gates are described in [`runner/README.md`](runner/README.md).
`k6/` and `echo-service/` contain the old load tests and are not part of this suite.

## Test binaries

| Binary (`integration/tests/`) | What it checks                                                     | In CI   | Local                       |
| ----------------------------- | ------------------------------------------------------------------ | ------- | --------------------------- |
| `integration.rs`              | 0-, 1- and 2-hop UDP loopback: at least 99% arrival, no corruption | yes     | `just integration-binchain` |
| `exit_origination.rs`         | the exit keeps sending when one return path cannot be resolved     | yes     | `just exit-origination`     |
| `pix.rs`                      | PIX settlement with `edgli` as the paying entry                    | v5 only | `LINE=v5 just pix`          |
| `pix_shapes.rs`               | PIX under end-user traffic shapes (needs `--pix-config`)           | v5 only | `LINE=v5 just pix-shapes`   |
| `return_path.rs`              | reply spread over relayers, and survival when a relayer dies       | v4 only | `just return-path`          |
| `upload_survival.rs`          | sustained upload; fails until hoprnet#8417 reaches the line        | no      | see `run.sh`                |
| `rotsee.rs`                   | the same pump against a funded Rotsee identity (`EDGLI_ROTSEE_*`)  | no      | `just rotsee`               |
| `profiling.rs`                | tokio-console and Perfetto traces, no pass/fail result             | no      | `just profile`              |

v5 is split in two tiers, chosen with `TIER` (see [`docs/v5-test-matrix.md`](docs/v5-test-matrix.md)
for what each test covers and costs): `pr` (integration, pix, the mixed shape) is the fast
happy-path set, `nightly` runs the rest. v4 runs everything on either tier. Which jobs each trigger
runs is listed under [CI](#ci).
Every run ends with a summary of each test, naming the step a failed test broke in.

CI runs 8 scenarios on v4 (6 of them `return_path`). Suites share one chain per
`run.sh` invocation, warm with frozen identities after the first. `return_path` runs last on v4,
on a fresh chain and one cluster per scenario, and only its 1-hop scenarios: the two-hop ones wait
for the v5 fix and `spread` is held out for asserting a ratio on a random draw. v5 waits on stack
fixes (branch `em/return-path-v5`).

Every scenario is `#[ignore]` because it needs external binaries. Thresholds are constants in
the test files, and there are no settings to change. Shared code is in `integration/src/`:
`cluster.rs` (localcluster), `env.rs` (`IntegrationEnv`, sessions), `pump.rs` (the traffic
pump, which returns `Transfer { mbps, arrival_pct(), sha_ok }`), `pix.rs` / `shapes.rs`, and
`pix_exit.rs` (parses the Exit's gate telemetry; ungated, so both lines' unit tests cover it).

## Running locally

```bash
just ci                         # what CI runs for the v4 line: resolve refs, build, run every suite
just ci-v5                      # the same for v5 (TIER=pr|nightly); PIX suites on x86_64-linux only

# A tier exactly as CI selects it, from binaries built here; works on darwin too. v5 builds the
# PIX hoprd + localcluster from HOPRD_SRC (a v5 hoprd git checkout, e.g. a `git worktree` of
# hoprd main), since the flake has no darwin one.
LINE=v5 HOPRD_SRC=../hoprd-main just tier pr        # integration, pix, mixed shape (~30 min)
LINE=v5 HOPRD_SRC=../hoprd-main just tier nightly   # exit_origination, the other shapes (~45 min)
just tier                                           # v4: every suite

just build build-chain          # hoprd + localcluster, bloklid + deployer + anvil (nix, cached)
just integration-binchain       # integration.rs on a fresh chain
just integration-binchain zero_one_two_hop

just cluster-up                 # terminal 1: a long-running cluster
just attach zero_one_two_hop    # terminal 2: run tests against it
just unit                       # unit tests, no cluster
just lint                       # fmt --check + clippy -D warnings
just clean                      # kill leftover processes, remove temp state
```

`LINE=v5` selects the line, and `just --list` shows every recipe. To override a single ref, use
`HOPRD_REF=` / `BLOKLI_REF=` (or `just hoprd_ref=…`). `HOPRNET_SHELL=path:../hoprnet` uses a
local hoprnet checkout for the dev shell. `just pix` builds hoprd from `HOPRD_SRC` (default
`../hoprd`), because the flake has no PIX binary for darwin.

The chain does not need docker: `scripts/integration/lib.sh chain_up` starts anvil, deploys the
contracts and starts bloklid, and localcluster connects to it with `--chain-url`.

| Env var                  | Meaning                                                                                     |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| `HOPRD_BIN`              | `hoprd` binary (default `result-hoprd/bin/hoprd`)                                           |
| `HOPRD_LOCALCLUSTER_BIN` | `hoprd-localcluster` binary                                                                 |
| `HOPRD_CHAIN_URL`        | bloklid to connect to (default `http://localhost:8080`)                                     |
| `HOPRD_CLUSTER_DATA_DIR` | use an already-running cluster instead of starting one                                      |
| `HOPR_LOG_DIR`           | parent directory for retained node/localcluster artifacts (`hoprd-it-*/logs/hoprd_<i>.log`) |
| `HOPRD_KEEP_ARTIFACTS=1` | keep per-node `hoprd_<i>.log` files after teardown                                          |
| `RUST_LOG`, `TEST_ARGS`  | CI uses `warn,hoprd_integration_test=info` and `--nocapture`                                |

To retain node logs under a fixed parent directory:

```bash
HOPR_LOG_DIR=/path/to/node-logs just integration-binchain
```

Each cluster gets a unique `hoprd-it-*` subdirectory containing `logs/hoprd_<i>.log`,
`localcluster.log`, and its state. Without this setting, the existing temporary-directory
behavior applies. This setting does not affect an attached cluster (`HOPRD_CLUSTER_DATA_DIR`).

anvil, bloklid and deployer logs are always written to `/tmp/hopr-chain/*.log`. `run.sh` sends nix
build output to `nix-build.log`, so a long build can look like it has stopped.

## CI

**`pr.yaml`** runs on every PR and in the merge queue, for both lines: title check, shell lint,
`fmt` + `clippy` (depot), and unit tests (the `hetzner` box). Before compiling, it pins `edgli`
the same way `run.sh` does (see below).

**`integration.yaml`** runs [`scripts/integration/run.sh`](scripts/integration/run.sh) on the
`hetzner` box. It is triggered by:

- upstream gates (hoprd, edge-client, blokli) through dispatch, on the line they request;
- this repo's merge queue, and PRs carrying a `run-integration*` label;
- a nightly run at 02:00 UTC (v4 gets no other automatic coverage, since merge queues only work on
  default branches);
- manual runs: `gh workflow run integration.yaml -f line=v5 -f tier=nightly -f project=hoprd -f rev=<sha>`.

Each trigger runs one job per line and tier, one after another on the box:

| Trigger                                   | v4  | v5 `pr` | v5 `nightly` |
| ----------------------------------------- | --- | ------- | ------------ |
| this repo's merge queue                   | yes | yes     | yes          |
| upstream dispatch / manual run, `line=v4` | yes | –       | –            |
| upstream dispatch / manual run, `line=v5` | –   | yes     | yes          |
| manual run, `line=v5 tier=pr`             | –   | yes     | –            |
| manual run, `line=v5 tier=nightly`        | –   | –       | yes          |
| nightly schedule                          | yes | –       | yes          |
| PR label `run-integration`                | –   | yes     | –            |
| PR label `run-integration-v4`             | yes | –       | –            |
| PR label `run-integration-v5-nightly`     | –   | –       | yes          |

PR labels add up, so `run-integration` plus `run-integration-v4` runs two jobs. The merge queue and
the upstream gates run every suite of their line, so a new or changed test passes before it lands.

No versions are stored. The project that triggered the run supplies its rev, and every other ref
is the current head of its branch on the line. `run.sh` builds hoprd, localcluster and the blokli
chain, pins `edgli`, runs each suite, and reports failures to Zulip with the versions that ran.

A green run records a fingerprint per line. The fingerprint covers the tests tree and the hoprd,
edge-client and blokli trees (trees, not commits: an upstream queue candidate is a new commit with
the PR head's tree). When a merge queue, this repo's or an upstream's dispatch, sees a fingerprint
that already passed in a run of the last 7 days, it skips that line. Usually that earlier run is the
PR's labelled run. If main or any upstream branch moved, the queue batched other PRs, or a
companion PR merged, the fingerprint changes and the line runs again.

### How edgli gets pinned

`scripts/integration/lib.sh pin_line_deps` resolves the edge-client ref: a companion PR head if
there is one, otherwise the line's branch head. It pins `edgli` to that commit, copies
edge-client's `hopr-lib` / `hopr-strategy` pins, and fails if the lock ends up with two copies of
either. `run.sh` and `pr.yaml` both run this step, so the committed locks only serve as the
default for local `cargo` / `just` runs. They never need to be pinned by hand.

As a result, a new edge-client commit can make this repo's checks fail even when nothing here
changed.

## Breaking upstream changes (companion PRs)

If an upstream PR breaks the tests, it cannot merge by itself, because its gate runs the tests
from `main`. To handle this, pair it with a tests PR and link the two in their descriptions:

```
Requires: hoprnet/hopr-integration-tests#50   # in the upstream PR
Requires: hoprnet/edge-client#186             # in the tests PR, one line per upstream PR
```

1. Open the tests PR with the adapted code and label it `run-integration` (add
   `run-integration-v4` / `run-integration-v5-nightly` for the other jobs). While an upstream PR
   is open, its line (`main` = v5, `release/*` = v4) uses that PR's head, both in `pr.yaml` and in
   the integration run.
2. Queue the upstream PRs. Each gate sends `tests_pr`, so it runs with the tests from the tests PR.
3. Queue the tests PR last. The merge queue refuses it while any `Requires:` target is still open.

A change that spans several repos uses the tests PR as the hub. The tests PR lists every upstream
PR, and each upstream PR lists only the tests PR. Until the last PR merges, other v5 queue entries
see a mixed stack, so run the queues one after another. `run.sh` warns when hoprd and edge-client
lock different hoprnet revisions.

Descriptions are read when a run starts, so after editing one, re-queue or re-label the PR. Fork
PRs are refused. A tests PR usually adapts:

- API changes in `HoprSessionClientConfig`, `PixEntryConfig` and `EdgeStrategyKind`. `env.rs`
  builds on both lines, so a field that exists on only one line goes behind `#[cfg(feature = "v5")]`.
- Values that depend on the payload size: `pix::PRICE_PER_BYTE` relative to `MAX_SSA_ALLOCATION`,
  and the geometry in `shapes.rs`. #53 (the 3246 B payload) is a worked example.

Check both lines before pushing:
`bash scripts/integration/with-v5-deps.sh cargo test --manifest-path integration/Cargo.toml --all-features --lib`,
then the same command without the wrapper for v4.
