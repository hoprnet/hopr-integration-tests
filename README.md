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
| `return_path.rs`              | reply spread over relayers, and survival when a relayer dies      | v4 only | `just return-path`          |
| `upload_survival.rs`          | sustained upload; fails until hoprnet#8417 reaches the line       | no      | see `run.sh`                |
| `rotsee.rs`                   | the same pump against a funded Rotsee identity (`EDGLI_ROTSEE_*`) | no      | `just rotsee`               |
| `profiling.rs`                | tokio-console and Perfetto traces, no pass/fail result            | no      | `just profile`              |

CI runs 9 scenarios on v4 (6 of them `return_path`) and 11 on v5. Suites share one chain per
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
| `HOPR_LOG_DIR`           | keep each run's logs in a folder under it (see below)        |
| `HOPRD_KEEP_ARTIFACTS=1` | keep per-node `hoprd_<i>.log` files, also when start-up fails |
| `RUST_LOG`, `TEST_ARGS`  | CI uses `warn,hoprd_integration_test=info` and `--nocapture` |

To keep the logs of each run:

```bash
HOPR_LOG_DIR=/path/to/logs just integration-binchain
```

Without this setting, the cluster lives in a temporary directory that is removed after the run.

### Logs of a run

With `HOPR_LOG_DIR` set, `run-binchain.sh` (`just integration-binchain`, `just integration-trace`)
gives each run its own folder `run-<time>-XXXX` under it, kept on success, failure and Ctrl-C:

| Path                  | Content                                                                   |
| --------------------- | ------------------------------------------------------------------------- |
| `hoprd-it-*/`         | node logs (`logs/hoprd_<i>.log`) and `localcluster.log`                   |
| `chain/`              | `anvil.log`, `bloklid.log`, `deployer.log`                                |
| `client-<target>.log` | the test output, stdout and stderr, including the edgli client logs       |
| `run.env`             | the settings the run used, the test repo revision, the exact binaries     |
| `trace-build.txt`     | with `just integration-trace`: the hoprd build and its `hopr-*` revisions |
| `run.status`          | `passed`, `failed` or `interrupted`, with the exit code                   |

- `run.env` is written from a fixed list of settings. It never copies the whole environment,
  API tokens or credentials in URLs.
- The client output is also printed to the console, and the run keeps the test's exit status.
  Tests run with `--nocapture`, so passing tests keep their client logs too.
- `CHAIN_SHARED=1`: the chain belongs to the caller and its logs cover other runs too, so they stay
  in `CHAIN_DATA_DIR`. `run.env` names that folder, and `chain-slice.txt` gives each log's size at
  the start and end of this run, so this run's part can be cut out (for example with `tail -c` and
  `head -c`).
- `HOPRD_CLUSTER_DATA_DIR` (attached cluster): its node logs stay in that folder, which `run.env`
  names. The client and chain logs still go to the run folder.

### TRACE logs

Release builds of hoprd have TRACE logs compiled out (hopr-lib sets
`tracing/release_max_level_debug`), so `RUST_LOG=...=trace` alone shows nothing more than DEBUG.
`just integration-trace` builds hoprd and hoprd-localcluster from a local hoprd checkout
(`HOPRD_SRC`, default `../hoprd`) with debug assertions on, which brings TRACE back while the
build stays optimised. It then runs the scenarios with each run's logs in its own folder under
`HOPR_LOG_DIR` (default `./logs`, see above):

```bash
RUST_LOG=info,hopr_protocol_session=trace just integration-trace zero_hop
```

- The binaries go to `<hoprd>/target/trace/`, apart from a normal release build.
- `trace-build.txt` (next to the binaries, and copied into the run folder) records the hoprd
  branch and revision, the build settings, and every `hopr-*` crate with its exact source.
- The checkout is built as it is. If it is not on the release line's hoprd branch
  (`release/4.1` for v4, `main` for v5), the build says so.
- PIX scenarios need `just pix`, which builds the PIX test strategy; this recipe does not.
- Debug assertions change the binary: extra checks run, and LTO is off. Use these runs to read
  logs, never as a performance baseline.
- If a run asked for TRACE but a binary could not provide it, the runner says so at the end.

### Exporting spans to a collector

Span export from the cluster nodes and the edgli client is off by default, so no run needs a
collector. To turn it on, start an OTLP collector and set `HOPRD_USE_OPENTELEMETRY=true` and its
endpoint for the run:

```bash
# For example Jaeger, which accepts OTLP over HTTP on 4318 and shows traces on 16686:
docker run --rm -p 4318:4318 -p 16686:16686 jaegertracing/all-in-one:latest

HOPRD_USE_OPENTELEMETRY=true HOPRD_OTLP_ENDPOINT=http://127.0.0.1:4318 \
  HOPR_LOG_DIR=$PWD/logs just integration-trace zero_hop
```

- The harness passes `HOPRD_USE_OPENTELEMETRY` on as given instead of forcing `false`. With it on,
  `HOPRD_OTEL_SIGNALS` defaults to `traces`; set it to change that.
- Every node span carries `run_id` (the run folder's name, or `HOPR_RUN_ID`), `component=hoprd`,
  and the node's `node_address` and `node_peer_id`. Extra labels go in `HOPRD_OTEL_EXPORT_LABELS`.
- `run.env` records these settings, without credentials.
- The edgli client in the test process exports its spans too, with the same switch and endpoint
  (OTLP over HTTP only), as service `edgli` with `run_id` and `component=edgli`. Its text logs
  still go to the console and `client-<target>.log` as before. Client and node spans are not
  joined into one trace, because HOPR packets carry no trace context; match them by `run_id`
  and time.
- Spans still waiting to be sent when a node stops can be lost: hoprd does not flush its exporter
  on SIGTERM, and localcluster stops its nodes with SIGKILL.
- The client sends its last spans when the test process exits. A test process that is killed
  (for example by Ctrl-C) loses the spans that were not sent yet.

Without `HOPR_LOG_DIR`, anvil, bloklid and deployer logs are written to `/tmp/hopr-chain/*.log`. `run.sh` sends nix
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

A green run records a fingerprint per line. The fingerprint covers the tests tree and the hoprd,
edge-client and blokli trees (trees, not commits: an upstream queue candidate is a new commit with
the PR head's tree). When a merge queue, this repo's or an upstream's dispatch, sees a fingerprint
that already passed in a run of the last 7 days, it skips that line. Usually that earlier run is the
PR's `run-integration` run. If main or any upstream branch moved, the queue batched other PRs, or a
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
