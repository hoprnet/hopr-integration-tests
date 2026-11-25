# Integration tests: inventory, coverage and tiers

What every test checks, what it costs in CI, which ones overlap, and which tier runs it.
Issues: #63 (combine overlapping scenarios), #65 (PR vs nightly tiers). Rows marked #82 / #83 / #109
come from those PRs.

Wall times are per scenario, setup included:

- **CI**: merge-queue run 40487443693.
- **L**: local runs of #82 (`return_path`, min–max over 3 runs).
- **#83**: the prototype runs in that PR's body.

After the first scenario on a chain, setup costs about 17 s (frozen 5-node cluster) or about 48 s
(8-node). The time goes into scenario bodies and PIX cycles: one profile cycle is 273 s nominal,
and the fill aims at 540 s.

## Tests

| ID  | Binary::test                                                                            | Origin       | Tier     | Wall                                                      | Nodes | Fault                                                                                      |
| --- | --------------------------------------------------------------------------------------- | ------------ | -------- | --------------------------------------------------------- | ----- | ------------------------------------------------------------------------------------------ |
| A1  | `integration::zero_one_two_hop` (was `one_hop` + `zero_hop`)                            | main         | pr       | CI 2.1m for the old two (+2.2m first bring-up), 2-hop new | 3     | –                                                                                          |
| B1  | `exit_origination::exit_should_keep_originating_…`                                      | main         | nightly  | CI 4.2m                                                   | 3     | abandoned session must not starve (hoprnet#8629)                                           |
| C1  | `pix::a_session_should_close_when_the_entry_can_no_longer_deposit`                      | main         | pr       | CI ~3.5m                                                  | 3     | entry budget exhausted                                                                     |
| C2  | `pix::edgli_entry_deposits_should_be_swept_into_the_exit_safe`                          | main         | pr       | CI ~2.3m                                                  | 3     | –                                                                                          |
| D2  | `pix_shapes::a_browsing_session_sustains_its_cycles`                                    | main         | nightly  | CI 9.2m                                                   | 3     | –                                                                                          |
| D3  | `pix_shapes::a_download_session_sustains_its_cycles`                                    | main         | nightly  | CI 9.0m                                                   | 3     | –                                                                                          |
| D4  | `pix_shapes::a_mixed_session_sustains_its_cycles` (+ former spike D1)                   | main         | pr       | CI 14.5m                                                  | 3     | –                                                                                          |
| D5  | `pix_shapes::an_idle_session_completes_its_cycle_on_exit_fill`                          | main         | nightly  | CI 10.6m                                                  | 3     | –                                                                                          |
| D6  | `pix_shapes::an_upload_session_completes_on_fill`                                       | main         | nightly  | CI 8.3m                                                   | 3     | –                                                                                          |
| E1  | `return_path::session_should_survive_return_relayer_loss`                               | #82          | nightly  | L 5.4–5.9m (first bring-up)                               | 5     | SIGKILL return relay (0,1)                                                                 |
| E2  | `return_path::session_should_survive_forward_relayer_loss`                              | #82          | nightly  | L 2.9–3.0m                                                | 5     | SIGKILL forward relay (1,0)                                                                |
| E3  | `return_path::a_symmetric_session_should_survive_relayer_loss`                          | #82          | nightly  | L 2.0–2.5m                                                | 5     | SIGKILL relay (1,1)                                                                        |
| E4  | `return_path::a_one_hop_…_exit_losing_its_return_relayer`                               | #82          | nightly  | L 5.3–5.4m (first 8-node)                                 | 8     | exit↔relay link cut (0,1)                                                                 |
| E5  | `return_path::a_one_hop_…_a_stalling_return_relayer`                                    | #82          | nightly  | L 3.2–3.3m                                                | 8     | exit↔relay link stall (0,1)                                                               |
| E6  | `return_path::session_should_survive_common_mode_return_outage`                         | #82          | nightly  | L 2.7–2.9m                                                | 5     | SIGSTOP all return relays, then thaw                                                       |
| E7  | `return_path::a_two_hop_…_exit_losing_its_return_relayer`                               | #82          | nightly  | L 2.7–2.8m                                                | 8     | link cut (0,2)                                                                             |
| E8  | `return_path::a_two_hop_session_should_survive_relayer_loss`                            | #82          | nightly  | L 2.7–2.9m                                                | 8     | SIGKILL (2,2)                                                                              |
| E9  | `return_path::a_three_hop_session_should_survive_relayer_loss`                          | #82          | nightly  | L 3.0–3.1m                                                | 8     | SIGKILL (3,3)                                                                              |
| E10 | `return_path::a_three_hop_session_should_survive_a_stalling_relayer`                    | #82          | nightly  | L 3.0–3.1m                                                | 8     | victim stalls every link (3,3)                                                             |
| E11 | `return_path::return_paths_should_spread_across_distinct_relayers`                      | main         | nightly  | L 0.4–0.6m                                                | 5     | –                                                                                          |
| E12 | `return_path::return_paths_should_track_the_tempered_score`                             | #82          | held out | L 0.4–0.6m (as E11)                                       | 5     | – (finding, hoprnet#8331)                                                                  |
| E13 | `return_path::an_idle_session_should_survive_a_long_common_mode_…`                      | #109         | nightly  | L 3.2–3.7m                                                | 5     | E6 after 35 s idle, 40 s, no settle                                                        |
| F1  | `pix_relay_loss::a_pix_cycle_survives_a_relay_death_on_both_paths`                      | #83          | nightly  | #83 7.5m (≈24m at production size)                        | 3     | kill half the return relays (1,1)                                                          |
| F2  | `pix_relay_loss::…_keeps_completing_cycles_after_a_return_relay_death` (+ former F2)    | #83          | nightly  | CI 17.5m (was 4.9m + 17.5m)                               | 3     | (0,1), 3 cycles; absorbed `…_forward_path_intact`                                          |
| F3  | `pix_relay_loss::…_deep_return_relay_death_and_the_generation_wipe`                     | #83          | nightly  | #83 8.6m                                                  | 4     | (0,2)                                                                                      |
| G1  | `pix_resume::a_session_resumed_within_the_window_keeps_its_paid_cycle`                  | hoprnet#8711 | nightly  | L 7.7–7.8m                                                | 3     | clean close, re-open under the same pseudonym at once                                      |
| G2  | `pix_resume::a_session_resumed_after_a_pause_inside_the_window_continues`               | hoprnet#8711 | nightly  | L 5.9m                                                    | 3     | as G1 after 0.6 x the resume window                                                        |
| G3  | `pix_resume::a_session_reopened_after_the_window_is_refused_and_a_fresh_pseudonym_pays` | hoprnet#8711 | nightly  | L 4.9–5.4m                                                | 3     | re-open after the window: refused `NotResumable`, a fresh pseudonym pays again (control)   |
| G4  | `pix_resume::a_session_dropped_without_a_close_is_taken_over_live`                      | hoprnet#8711 | nightly  | L 7.8–10.1m                                               | 3     | as G1 but dropped without a close: the Exit never detaches, the live Session is taken over |

G1-G4 were measured locally, twice each, cluster bring-up included. They need a localcluster that
takes `resume_window` in `--pix-config` (hoprd#293) and a hoprnet with the resume window (hoprnet#8711)
in both the Exit and the edgli Entry; the tier fails on a localcluster without the key.

Not in CI: upload_survival (T24 with a shrunk opener cache; fails end to end on hoprnet master
`2acc3a5` although it carries hoprnet#8417: 15 % echoed, 13 s stall), rotsee, profiling.

## gnosis_vpn-testenv tests (#49)

`wireguard.rs` runs gnosis_vpn-testenv's diagnostic tests on the HOPR layer: gnosis_vpn-client's
WireGuard session (`IntegrationEnv::open_vpn_session`: 1 hop, `Segmentation | NoDelay`, maxed-out
SURBs, the ping-to-main SURB ramp, robust flow control, PIX off). Its far end echoes, sinks or pushes
WireGuard-sized datagrams, or terminates TCP: smoltcp runs at both ends of the tunnel, one IP packet
per 1452 B datagram, with ICMP pings next to the transfer. Tests use testenv's thresholds. One
3-node cluster with a flat 1 ms latency profile, so `delay_link` can impair a link at runtime.

| testenv                                       | `wireguard::`                                                            |
| --------------------------------------------- | ------------------------------------------------------------------------ |
| T04 fixed-throughput, T05 loaded-latency, T06 upload and download arms | `short_tcp_transfers_should_keep_their_rate_and_latency` (5, 20 MB each way, **pr**), `long_tcp_…` (200, 500 MB, nightly) |
| T07 cold-start, T15 warmup-knee               | `a_fresh_session_should_reach_full_rate_within_its_ramp`                 |
| T09 impairment-ladder                         | `return_relays_of_unequal_latency_should_carry_the_session` (equal 25/50 ms, gap 50, far 100; the flat rung is every other test) |
| T10 forced-reconnect                          | `a_session_reopened_under_inbound_traffic_should_carry_it`               |
| T11 capability-matrix                         | `the_exit_should_shape_exactly_the_rate_controlled_sessions`             |
| T12 balancer-sweep                            | `every_surb_upstream_should_carry_downloads`                             |
| T18 capacity-ceiling, T06 after-bulk arm      | `a_download_after_transfers_should_hold_up_to_the_capacity_knee` (one session: 3 transfers, then the rungs) |
| T19 background-load                           | `a_trickle_on_another_session_should_not_starve_a_transfer`              |
| T22 concurrent-clients                        | `concurrent_sessions_should_share_the_exit` (1 and 10 TCP downloads)     |
| T23 sustained-soak, T24 sustained-upload, T06 echo and idle arms, T07's 75 s idle finding | `a_long_session_should_hold_its_loss_and_memory` (600 s call, a transfer a minute, 3 Mbit/s TCP upload, a call after 120 s idle) |
| T31 frame-forensics                           | every `wireguard` datagram stream: a read that is not one whole datagram fails it |

TCP transfers pass on rate (≥ 7 Mbit/s), no gap over 5 s, a mean ping RTT under 1.5 s and a jitter
(mean change between consecutive RTTs) between one mixer's mean delay and 250 ms.

Already covered here before #49: T08's split (E11), T20 (E1–E13; its packet-loss ladder needs a loss
rule the localcluster relays do not have), T30 (A1).

T13 (MTU sweep) needs no cluster: whole datagrams per tunnel MTU are a Session property, so it
goes to hoprnet's `transport/session` tests (hoprnet#8716).

Left in testenv, because the cause is not in the HOPR layer or the test is not a transport test: T01,
T02, T03, T14, T16, T17, T21 (stack and client state), the client-side halves of T10 (WireGuard peer
removal, liveness watchdog) and T12 (the ping-tier mask), T25–T29, T32, T33 (runbook and production).

| Set (test time, build excluded) | Before                   | After                                                                   |
| ------------------------------- | ------------------------ | ----------------------------------------------------------------------- |
| PR label `run-integration`      | 75m (~110m with #82/#83) | **pr: 26m in CI**                                                       |
| nightly tier                    | –                        | ~41m, ~104m with #82/#83/#109, ~132m with #116, ~198m with `wireguard`  |

## Features

| ID    | Feature                                                           | Asserted as                                                                                           |
| ----- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| S0    | 0-hop session                                                     | 10 MiB, ≥99 % back, SHA intact                                                                        |
| S1    | 1-hop (1,1) session                                               | 10 MiB, ≥99 %, SHA, no latency profile                                                                |
| S1r/f | 1-hop return-only (0,1) / forward-only (1,0)                      | baseline >50 %, latency profile                                                                       |
| S2    | 2-hop session                                                     | 10 MiB, ≥99 %, SHA (A1); baseline >50 % inside fault scenarios                                        |
| S3    | 3-hop session                                                     | baseline >50 %, inside fault scenarios only                                                           |
| S4    | Return-path relayer spread (hoprnet#8331)                         | ≥ cand−1 active relayers, imbalance ≤3 (not gated)                                                    |
| R1    | Return relay death; entry reassembly recovers                     | recovery ≤15 s beyond the healthy ramp, no stall >15 s, steady state, SHA intact: the recovery checks |
| R2    | Forward relay death; exit sequencer recovers                      | the recovery checks                                                                                   |
| R3    | Relay death on both paths at 1 / 2 / 3 hops                       | the recovery checks                                                                                   |
| R4    | Exit-only detection: exit↔relay link cut (1 / 2 hops)            | the recovery checks                                                                                   |
| R5    | Stall that never disconnects                                      | the recovery checks                                                                                   |
| R6    | Common-mode outage                                                | session not closed during the outage, then the recovery checks                                        |
| X1    | Exit keeps originating while one session is SURB-starved          | ingress and origination counters move, canary ≥50 %, no starvation log line                           |
| P1    | PIX demo cycle (32 packets): deposit → confirm → recover → sweep  | ≥4 cycles, whole-multiple accounting, Safe drop ≥ recovered, 0 failed / over-budget / timed out       |
| P2    | PIX budget exhaustion                                             | exactly 2 deposits, over-budget ≥1, Exit times out the deposit, Session not Complete                  |
| P3    | Production-profile geometry (40 960-packet cycle, `--pix-config`) | Exit observable, cycle swept within 3 nominal cycles                                                  |
| P4    | Exit fill completes a cycle; successor funded                     | keys ≥1, confirmed ≥2                                                                                 |
| P5    | Traffic shapes: browsing, download, upload                        | per-shape arrival, intact, sweeps                                                                     |
| P6    | Shape transitions: app → quiet/fill → bulk                        | bulk >50 % after quiet, sweeps ≥2                                                                     |
| P7    | Exit gate serves the surplus; Exit is paid                        | surplus run ≥2048, no `share_lag`; Safe delta ≥ n × per_cycle                                         |
| P8    | No share-bearing SURB evicted (`ring_share`, #83)                 | eviction delta = 0                                                                                    |
| P9    | PIX cycle survives relay death, generation wipe included (#83)    | cycle swept and paid after killing half the return relays                                             |

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

| ID  | Change                                                                                 | Status            | Saves          | Coverage change                                               |
| --- | -------------------------------------------------------------------------------------- | ----------------- | -------------- | ------------------------------------------------------------- |
| K1  | `zero_one_two_hop`: one 3-node cluster, steps 1-hop, 0-hop, 2-hop                      | done              | one edgli boot | **gains** a strict healthy 2-hop (S2)                         |
| K2  | Spike D1 folded into mixed D4; its `observable()` check moves into D4's `setup` step   | done              | **−7.1m**      | none: D4's bulk phase is the same cycle with stricter asserts |
| K3  | E11 (spread) becomes a logged metric of E1's warm-up                                   | for #82           | −0.5m          | none, it is not gated                                         |
| K4  | Run return_path on PIX sessions with the cycle asserts, dropping F1–F3 or E1/E3/E7     | rejected          | −20m           | plain sessions still run, so both families stay               |
| K5  | Chain recoverable faults on one cluster (E6 then E1; E4 then E5)                       | open, low value   | ~−2m           | victim and baseline conditions change                         |
| K6  | Run D2, D3, D5, D6 concurrently on distinct exits (separate edgli, per-exit telemetry) | open, spike first | −30m nightly   | none                                                          |
| K7  | Run the 5-node scenarios on the 8-node identities                                      | open              | ~−3m           | 4 → 7 candidates, so thresholds need re-deriving              |

## Tiers

| Tier      | Trigger                                                      | Suites                                                                                                        |
| --------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| `pr`      | `run-integration` label, merge queue, dispatches, `schedule` | integration, dead_last_hop, pix, pix_shapes mixed, wireguard short TCP                                        |
| `nightly` | `run-integration-nightly` label, `schedule`                  | exit_origination, the other four pix_shapes, return_path (all but E12), pix_relay_loss, pix_resume, wireguard |

- `run.sh` picks the suites from `TIER`.
- The `nightly` tier runs as two jobs on two runners: `SHARD=a` (exit_origination, wireguard,
  pix_relay_loss, ~90m) and `SHARD=b` (pix_shapes, return_path, pix_resume, ~105m). Every trigger of
  `nightly`, the label included, gets both. No `SHARD` runs the whole tier.
- Each tier is its own job. The merge queue and dispatches run `pr` only; the schedule runs both
  tiers, and the label runs `nightly` on a PR before it is queued. The full table is in the README, under CI.
- The pass marker's fingerprint carries the tier and shard, so a merge gate never reuses a nightly green.

## Measured locally

`just tier` on an Apple-silicon Mac, 2026-10-28. Stack: hoprd `main` (flake) plus the PIX hoprd
built from `8d9f11a`, edge-client `main` head, blokli `main`. Each tier ran on one fresh
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
only, so the recipe builds the PIX hoprd and the `--pix-config` localcluster from
`HOPRD_SRC`, a hoprd git checkout, and pins edgli to edge-client's `main` head like CI. It has to
be a git checkout: nix copies a plain directory into the store whole, `target/` included.

```bash
git -C ../hoprd worktree add ../hoprd-main origin/main   # once
HOPRD_SRC=../hoprd-main just tier pr
HOPRD_SRC=../hoprd-main just tier nightly
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
