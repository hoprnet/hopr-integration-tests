# v5 integration tests: inventory, coverage and tiers

What every v5 test checks, what it costs in CI, which ones overlap, and which tier runs it.
Issues: #63 (combine overlapping scenarios), #65 (PR vs nightly tiers). Rows marked #82 / #83 come
from those PRs and are not on `main` yet.

Wall times are per scenario, setup included:

- **CI**: merge-queue run 40487443693, v5 job.
- **L**: local v5 runs of #82 (`return_path`, min–max over 3 runs).
- **#83**: the prototype runs in that PR's body.

After the first scenario on a chain, setup costs about 17 s (frozen 5-node cluster) or about 48 s
(8-node). The time goes into scenario bodies and PIX cycles: one profile cycle is 273 s nominal,
and the fill aims at 540 s.

## Tests

| ID  | Binary::test                                                        | Origin | Tier     | Wall                                                      | Nodes | Fault                                |
| --- | ------------------------------------------------------------------- | ------ | -------- | --------------------------------------------------------- | ----- | ------------------------------------ |
| A1  | integration::zero_one_two_hop (was `one_hop` + `zero_hop`)          | main   | pr       | CI 2.1m for the old two (+2.2m first bring-up), 2-hop new | 3     | –                                    |
| B1  | exit*origination::exit_should_keep_originating*…                    | main   | nightly  | CI 4.2m                                                   | 3     | leaked SURB-starved session          |
| C1  | pix::a_session_should_close_when_the_entry_can_no_longer_deposit    | main   | pr       | CI ~3.5m                                                  | 3     | entry budget exhausted               |
| C2  | pix::edgli_entry_deposits_should_be_swept_into_the_exit_safe        | main   | pr       | CI ~2.3m                                                  | 3     | –                                    |
| D2  | pix_shapes::a_browsing_session_sustains_its_cycles                  | main   | nightly  | CI 9.2m                                                   | 3     | –                                    |
| D3  | pix_shapes::a_download_session_sustains_its_cycles                  | main   | nightly  | CI 9.0m                                                   | 3     | –                                    |
| D4  | pix_shapes::a_mixed_session_sustains_its_cycles (+ former spike D1) | main   | pr       | CI 14.5m                                                  | 3     | –                                    |
| D5  | pix_shapes::an_idle_session_completes_its_cycle_on_exit_fill        | main   | nightly  | CI 10.6m                                                  | 3     | –                                    |
| D6  | pix_shapes::an_upload_session_completes_on_fill                     | main   | nightly  | CI 8.3m                                                   | 3     | –                                    |
| E1  | return_path::session_should_survive_return_relayer_loss             | #82    | nightly  | L 5.4–5.9m (first bring-up)                               | 5     | SIGKILL return relay (0,1)           |
| E2  | return_path::session_should_survive_forward_relayer_loss            | #82    | nightly  | L 2.9–3.0m                                                | 5     | SIGKILL forward relay (1,0)          |
| E3  | return_path::a_symmetric_session_should_survive_relayer_loss        | #82    | nightly  | L 2.0–2.5m                                                | 5     | SIGKILL relay (1,1)                  |
| E4  | return*path::a_one_hop*…\_exit_losing_its_return_relayer            | #82    | nightly  | L 5.3–5.4m (first 8-node)                                 | 8     | exit↔relay link cut (0,1)           |
| E5  | return*path::a_one_hop*…\_a_stalling_return_relayer                 | #82    | nightly  | L 3.2–3.3m                                                | 8     | exit↔relay link stall (0,1)         |
| E6  | return_path::session_should_survive_common_mode_return_outage       | #82    | nightly  | L 2.7–2.9m                                                | 5     | SIGSTOP all return relays, then thaw |
| E7  | return*path::a_two_hop*…\_exit_losing_its_return_relayer            | #82    | nightly  | L 2.7–2.8m                                                | 8     | link cut (0,2)                       |
| E8  | return_path::a_two_hop_session_should_survive_relayer_loss          | #82    | nightly  | L 2.7–2.9m                                                | 8     | SIGKILL (2,2)                        |
| E9  | return_path::a_three_hop_session_should_survive_relayer_loss        | #82    | nightly  | L 3.0–3.1m                                                | 8     | SIGKILL (3,3)                        |
| E10 | return_path::a_three_hop_session_should_survive_a_stalling_relayer  | #82    | nightly  | L 3.0–3.1m                                                | 8     | victim stalls every link (3,3)       |
| E11 | return_path::return_paths_should_spread_across_distinct_relayers    | main   | held out | L 0.4–0.6m                                                | 5     | –                                    |
| F1  | pix_relay_loss::a_pix_cycle_survives_a_relay_death_on_both_paths    | #83    | nightly  | #83 7.5m (≈24m at production size)                        | 3     | kill half the return relays (1,1)    |
| F2  | pix_relay_loss::…\_return_relay_death_with_the_forward_path_intact  | #83    | nightly  | #83 7.6m                                                  | 3     | (0,1)                                |
| F3  | pix_relay_loss::…\_deep_return_relay_death_and_the_generation_wipe  | #83    | nightly  | #83 8.6m                                                  | 4     | (0,2)                                |

Not in v5 CI: upload_survival, rotsee, profiling.

| Set (test time, build excluded)                | Before                   | After                    |
| ---------------------------------------------- | ------------------------ | ------------------------ |
| v5 gate (PR, merge queue, upstream dispatches) | 75m (~110m with #82/#83) | **pr: ~27m**             |
| v5 nightly                                     | –                        | ~41m, ~100m with #82/#83 |

## Features

| ID    | Feature                                                           | Asserted as                                                                                     |
| ----- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| S0    | 0-hop session                                                     | 10 MiB, ≥99 % back, SHA intact                                                                  |
| S1    | 1-hop (1,1) session                                               | 10 MiB, ≥99 %, SHA, no latency profile                                                          |
| S1r/f | 1-hop return-only (0,1) / forward-only (1,0)                      | baseline >50 %, latency profile                                                                 |
| S2    | 2-hop session                                                     | 10 MiB, ≥99 %, SHA (A1); baseline >50 % inside fault scenarios                                  |
| S3    | 3-hop session                                                     | baseline >50 %, inside fault scenarios only                                                     |
| S4    | Return-path relayer spread (hoprnet#8331)                         | ≥ cand−1 active relayers, imbalance ≤3 (not gated)                                              |
| R1    | Return relay death; entry reassembly recovers                     | recovery ≤15 s beyond the healthy ramp, no stall >15 s, steady state, SHA (R0–R6)               |
| R2    | Forward relay death; exit sequencer recovers                      | R0–R6                                                                                           |
| R3    | Relay death on both paths at 1 / 2 / 3 hops                       | R0–R6                                                                                           |
| R4    | Exit-only detection: exit↔relay link cut (1 / 2 hops)            | R0–R6                                                                                           |
| R5    | Stall that never disconnects                                      | R0–R6                                                                                           |
| R6    | Common-mode outage                                                | session not closed during the outage, then R0–R6                                                |
| X1    | Exit keeps originating while one session is SURB-starved          | ingress and origination counters move, canary ≥50 %, starvation log line seen                   |
| P1    | PIX demo cycle (32 packets): deposit → confirm → recover → sweep  | ≥4 cycles, whole-multiple accounting, Safe drop ≥ recovered, 0 failed / over-budget / timed out |
| P2    | PIX budget exhaustion                                             | exactly 2 deposits, over-budget ≥1, Exit times out the deposit, Session not Complete            |
| P3    | Production-profile geometry (40 960-packet cycle, `--pix-config`) | Exit observable, cycle swept within 3 nominal cycles                                            |
| P4    | Exit fill completes a cycle; successor funded                     | keys ≥1, confirmed ≥2                                                                           |
| P5    | Traffic shapes: browsing, download, upload                        | per-shape arrival, intact, sweeps                                                               |
| P6    | Shape transitions: app → quiet/fill → bulk                        | bulk >50 % after quiet, sweeps ≥2                                                               |
| P7    | Exit gate serves the surplus; Exit is paid                        | surplus run ≥2048, no `share_lag`; Safe delta ≥ n × per_cycle                                   |
| P8    | No share-bearing SURB evicted (`ring_share`, #83)                 | eviction delta = 0                                                                              |
| P9    | PIX cycle survives relay death, generation wipe included (#83)    | cycle swept and paid after killing half the return relays                                       |

## Coverage

S = strict assertion, w = exercised but weakly or not asserted.

| Feature | A1  | B1  | C1  | C2  | D2  | D3  | D4  | D5  | D6  | E1  | E2  | E3  | E4  | E5  | E6  | E7  | E8  | E9  | E10 | E11 | F1  | F2  | F3  |
| ------- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S0      | S   | w   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| S1      | S   |     | w   | w   | w   | w   | w   | w   | w   |     |     | w   |     |     |     |     |     |     |     |     | w   |     |     |
| S1r/f   |     |     |     |     |     |     |     |     |     | w   | w   |     | w   | w   | w   |     |     |     |     | w   |     | w   |     |
| S2      | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     | w   | w   |     |     |     |     |     | w   |
| S3      |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     | w   | w   |     |     |     |     |
| S4      |     |     |     |     |     |     |     |     |     | w   |     |     |     |     | w   |     |     |     |     | S   |     |     |     |
| R1      |     |     |     |     |     |     |     |     |     | S   |     | S   |     |     |     |     | S   | S   |     |     |     |     |     |
| R2      |     |     |     |     |     |     |     |     |     |     | S   | S   |     |     |     |     | S   | S   |     |     |     |     |     |
| R4      |     |     |     |     |     |     |     |     |     |     |     |     | S   |     |     | S   |     |     |     |     |     |     |     |
| R5      |     |     |     |     |     |     |     |     |     |     |     |     |     | S   |     |     |     |     | S   |     |     |     |     |
| R6      |     |     |     |     |     |     |     |     |     |     |     |     |     |     | S   |     |     |     |     |     |     |     |     |
| X1      |     | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| P1      |     |     | w   | S   | w   | w   | w   | w   | w   |     |     |     |     |     |     |     |     |     |     |     | w   | w   | w   |
| P2      |     |     | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| P3      |     |     |     |     | w   | w   | S   | w   | w   |     |     |     |     |     |     |     |     |     |     |     | w   | w   | w   |
| P4      |     |     |     |     |     |     |     | S   | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| P5      |     |     |     |     | S   | S   |     |     | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| P6      |     |     |     |     |     |     | S   |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |
| P7      |     |     |     |     | S   | S   | S   | S   | S   |     |     |     |     |     |     |     |     |     |     |     | S   | S   | S   |
| P8      |     |     |     |     | S   | S   | S   | S   | S   |     |     |     |     |     |     |     |     |     |     |     | S   | S   | S   |
| P9      |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     |     | S   | S   | S   |

## Combining

| ID  | Change                                                                                 | Status            | Saves (v5)     | Coverage change                                               |
| --- | -------------------------------------------------------------------------------------- | ----------------- | -------------- | ------------------------------------------------------------- |
| K1  | `zero_one_two_hop`: one 3-node cluster, steps 1-hop, 0-hop, 2-hop                      | done              | one edgli boot | **gains** a strict healthy 2-hop (S2)                         |
| K2  | Spike D1 folded into mixed D4; its `observable()` check moves into D4's `setup` step   | done              | **−7.1m**      | none: D4's bulk phase is the same cycle with stricter asserts |
| K3  | E11 (spread) becomes a logged metric of E1's warm-up                                   | for #82           | −0.5m          | none, it is not gated                                         |
| K4  | Run return_path on PIX sessions with the cycle asserts, dropping F1–F3 or E1/E3/E7     | rejected          | −20m           | v5 still runs plain sessions, so both families stay           |
| K5  | Chain recoverable faults on one cluster (E6 then E1; E4 then E5)                       | open, low value   | ~−2m           | victim and baseline conditions change                         |
| K6  | Run D2, D3, D5, D6 concurrently on distinct exits (separate edgli, per-exit telemetry) | open, spike first | −30m nightly   | none                                                          |
| K7  | Run the 5-node scenarios on the 8-node identities                                      | open              | ~−3m           | 4 → 7 candidates, so thresholds need re-deriving              |

## Tiers

| Tier      | Trigger                                                                   | v5 suites                                                                                     |
| --------- | ------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `pr`      | `run-integration` label, merge queue, upstream dispatches, manual default | integration, pix, pix_shapes mixed                                                            |
| `nightly` | `schedule`, or a dispatch with `tier: nightly`                            | exit_origination, the other four pix_shapes; return_path and pix_relay_loss once #82/#83 land |

- `run.sh` picks the suites from `TIER`. v4 ignores it and runs everything, as before.
- The schedule runs v4, v5 `pr` and v5 `nightly` as three jobs one after another, so a night covers
  every suite and each job stays under the 150-minute timeout.
- The pass marker's fingerprint carries the tier, so a merge gate never reuses a nightly green.

## Measured locally

`just tier` on an Apple-silicon Mac, 2026-10-28. Stack: hoprd `main` (flake) plus the PIX hoprd
built from `8d9f11a`, edge-client `main` head, blokli `release/0.14`. Each tier ran on one fresh
chain. Both tiers were green.

| Tier    | Suite            | Tests | Test time | Summary                      |
| ------- | ---------------- | ----- | --------- | ---------------------------- |
| pr      | integration      | 1     | 4.6m      |                              |
| pr      | pix              | 2     | 6.5m      |                              |
| pr      | pix_shapes mixed | 1     | 15.4m     |                              |
| pr      | **total**        | 4     |           | 4 passed, 0 failed in 28m57s |
| nightly | exit_origination | 1     | 5.5m      |                              |
| nightly | pix_shapes       | 4     | 38.0m     |                              |
| nightly | **total**        | 5     |           | 5 passed, 0 failed in 44m24s |

The summary time includes the local build checks and the chain start.

## Running a tier locally

`just tier <pr|nightly>` runs the same suites CI does (`run_tier` in `scripts/integration/lib.sh`,
shared with `run.sh`) from binaries built on the machine. The flake's PIX hoprd is x86_64-linux
only, so on v5 the recipe builds the PIX hoprd and the `--pix-config` localcluster from
`HOPRD_SRC`, a v5 hoprd git checkout, and pins edgli to edge-client's line head like CI. It has to
be a git checkout: nix copies a plain directory into the store whole, `target/` included.

```bash
git -C ../hoprd worktree add ../hoprd-main origin/main   # once
LINE=v5 HOPRD_SRC=../hoprd-main just tier pr
LINE=v5 HOPRD_SRC=../hoprd-main just tier nightly
```

The run ends with the same summary CI prints. One suite or scenario: `just pix-shapes <name>`,
`just integration-binchain`, `just exit-origination`.

## Reporting

Every run ends with a pytest-style summary on the console, in the job summary, and (failures only)
in the Zulip message:

```
═══════ summary ═══════
PASSED integration::zero_one_two_hop
FAILED pix_shapes::a_mixed_session_sustains_its_cycles
       step `bulk`: the Session did not carry traffic again after going quiet …
1 passed, 1 failed in 31m02s
```

Tests name their phases with `hoprd_integration_test::step`. A failing step fails the test with
the step's name, and the step is recorded in `IT_STEP_LOG`. `run-binchain.sh` records each libtest
`test … ok|FAILED` line in `IT_RESULTS`, together with the test's own `Error:` line or panic
message. `print_summary` (`scripts/integration/lib.sh`) joins the two.

Existing tools were evaluated first:

| Tool                                      | Why not                                                                                                              |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| cucumber 0.23                             | Gherkin plus attribute-matched steps, `harness = false`, a World per scenario; its JUnit failure drops the step name |
| cargo-nextest                             | process per test breaks the shared `OnceCell` cluster; JUnit per invocation, no steps                                |
| libtest `--format junit/json`             | `-Z unstable-options` (stable only via `RUSTC_BOOTSTRAP`), stdout clashes with `--nocapture`, bare failure text      |
| libtest-mimic, rstest(-bdd), test-case    | no sequential named steps with failure reporting                                                                     |
| allure-rs                                 | steps exist, but needs the Allure CLI, low adoption, not GitHub-native                                               |
| JUnit GitHub actions (mikepenz, dorny, …) | need the libtest JUnit above, plus `checks: write`                                                                   |
