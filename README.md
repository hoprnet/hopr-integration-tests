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

| Binary (`integration/tests/`) | What it checks                                                    | In CI   | Local                       |
| ----------------------------- | ----------------------------------------------------------------- | ------- | --------------------------- |
| `integration.rs`              | 0-hop and 1-hop UDP loopback: at least 99% arrival, no corruption | yes     | `just integration-binchain` |
| `exit_origination.rs`         | the exit keeps sending when one return path cannot be resolved    | yes     | `just exit-origination`     |
| `pix.rs`                      | PIX settlement with `edgli` as the paying entry                   | v5 only | `LINE=v5 just pix`          |
| `pix_shapes.rs`               | PIX under end-user traffic shapes (needs `--pix-config`)          | v5 only | `LINE=v5 just pix-shapes`   |
| `return_path.rs`              | reply spread over relayers, and survival when a relayer dies      | no      | `just return-path`          |
| `upload_survival.rs`          | sustained upload; fails until hoprnet#8417 reaches the line       | no      | see `run.sh`                |
| `rotsee.rs`                   | the same pump against a funded Rotsee identity (`EDGLI_ROTSEE_*`) | no      | `just rotsee`               |
| `profiling.rs`                | tokio-console and Perfetto traces, no pass/fail result            | no      | `just profile`              |

`return_path` is excluded from CI because its assertions depend on a random relayer choice, so a
failure does not point to a bug. CI runs 3 scenarios on v4 and 11 on v5, each on a fresh chain.

Every scenario is `#[ignore]` because it needs external binaries. Thresholds are constants in
the test files, and there are no settings to change. Shared code is in `integration/src/`:
`cluster.rs` (localcluster), `env.rs` (`IntegrationEnv`, sessions), `pump.rs` (the traffic
pump, which returns `Transfer { mbps, arrival_pct(), sha_ok }`) and `pix.rs` / `shapes.rs`.

## Running locally

```bash
just ci                         # what CI runs for the v4 line: resolve refs, build, run every suite
just ci-v5                      # the same for v5, PIX included

just build build-chain          # hoprd + localcluster, bloklid + deployer + anvil (nix, cached)
just integration-binchain       # integration.rs on a fresh chain
just integration-binchain zero_hop

just cluster-up                 # terminal 1: a long-running cluster
just attach one_hop             # terminal 2: run tests against it
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

| Env var                  | Meaning                                                      |
| ------------------------ | ------------------------------------------------------------ |
| `HOPRD_BIN`              | `hoprd` binary (default `result-hoprd/bin/hoprd`)            |
| `HOPRD_LOCALCLUSTER_BIN` | `hoprd-localcluster` binary                                  |
| `HOPRD_CHAIN_URL`        | bloklid to connect to (default `http://localhost:8080`)      |
| `HOPRD_CLUSTER_DATA_DIR` | use an already-running cluster instead of starting one       |
| `HOPRD_KEEP_ARTIFACTS=1` | keep per-node `hoprd_<i>.log` files after teardown           |
| `RUST_LOG`, `TEST_ARGS`  | CI uses `warn,hoprd_integration_test=info` and `--nocapture` |

anvil, bloklid and deployer logs are always written to `/tmp/hopr-chain/*.log`. `run.sh` sends nix
build output to `nix-build.log`, so a long build can look like it has stopped.

## CI

**`pr.yaml`** runs on every PR and in the merge queue, for both lines: title check, shell lint,
`fmt` + `clippy` (depot), and unit tests (the `hetzner` box). Before compiling, it pins `edgli`
the same way `run.sh` does (see below).

**`integration.yaml`** runs [`scripts/integration/run.sh`](scripts/integration/run.sh) on the
`hetzner` box. It is triggered by:

- upstream gates (hoprd, edge-client, blokli) through dispatch, on the line they request;
- this repo's merge queue, and PRs labelled `run-integration`, on both lines;
- a nightly run at 02:00 UTC on v4, which is the only automatic v4 coverage, since merge queues
  only work on default branches;
- manual runs: `gh workflow run integration.yaml -f line=v5 -f project=hoprd -f rev=<sha>`.

No versions are stored. The project that triggered the run supplies its rev, and every other ref
is the current head of its branch on the line. `run.sh` builds hoprd, localcluster and the blokli
chain, pins `edgli`, runs each suite, and reports failures to Zulip with the versions that ran.

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

1. Open the tests PR with the adapted code and label it `run-integration`. While an upstream PR
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
