# PIX idle fill: implementation plan for `hoprnet`

Status: proposal, 2026-09-07. Written against hoprnet `85e698a7` (the rev edgli, hoprd and
hoprd-test all pin). File and line references below are to that rev.

## 0. The contract this implements

A PIX Exit guarantees that every **funded** SSA cycle's return packets are delivered before the
cycle's recovery deadline. Application replies count towards that; whatever they do not cover, the
Exit delivers itself as keep-alive packets ("fill"). The Entry, by opening a PIX Session with a SURB
balancer, agrees to keep supplying SURBs at the rate this needs and to fund the successor cycles it
produces. The result is a minimum spend for an idle client of one quota per recovery deadline, and
no stranded deposits on either side as long as the Entry keeps supplying SURBs.

What does not change: every existing supervisor deadline and the service gate. Fill sits in front
of the backstop. A client that stops supplying SURBs, whose return path is broken, or whose
balancer is off still hits `max_recovery_time` (or `RecoveryIdle`) exactly as today.

## 1. Design

### 1.1 Terms

| term | meaning |
| --- | --- |
| E | packets one cycle emits, `polys × (threshold + surplus)`; 327 680 at 4096 × 64 + 16 |
| organic egress | Exit → Entry data packets, the ones that pass the service gate today |
| fill | Exit-originated keep-alive packets, one SURB and one share each, sent only to advance a cycle |
| heartbeat | the fill floor while organic egress already covers the need (default one per 60 s) |
| floor rate | `E / max_recovery_time`, about 46 packets/s at the shipped 2 h deadline and production geometry |

### 1.2 Rate law

Evaluated once per `sampling_interval` for the cycle at the paid front while it is `Recovering`
with its recovery clocks armed (`PerSsaState::recovery_hard_deadline.is_some()`), i.e. funded and
actually being served. Nowhere else: an unfunded front gets no fill, and a cycle behind the front
cannot be fed because emission is clamped to one cycle.

```text
remaining   = (E − largest_shares_seen(front) + predecessor_tail) × (1 + loss_margin)
finish_by   = clock_start + finish_fraction × max_recovery_time
time_left   = max(finish_by − now, sampling_interval)
required    = ceil(remaining / time_left)               packets/s
organic     = EMA over organic_window of Δ gate.served_total() / Δt
fill        = clamp(required − organic, heartbeat_rate, max_rate)
```

- `largest_shares_seen` is what the supervisor already tracks per cycle
  (`supervisor.rs:56`, updated in `on_recovery_progress`). It counts surplus shares, so
  `E − seen` is the packets still to be spent, not the useful shares still needed. That
  overestimates slightly near the end of a cycle, which is the safe direction.
- `predecessor_tail` is the recovered predecessor's unspent surplus still queued ahead in the
  Entry's FIFO buffer: `max_shares_seen(pred) − paid_recovery_tail.largest_shares_seen`. Those SURBs
  have to be spent before the front's shares reach the reconstructor, so they are part of the work.
- `organic` is read off `ServiceGate::served_total()` (`gate.rs:188`). Fill packets do **not** pass
  the gate (see 1.4), so that counter stays organic-only and no subtraction is needed.
- `finish_fraction` below 1 is the safety margin against ack latency, loss, and the
  `unused_verifier_lifetime` interplay. 0.75 leaves 30 minutes at the shipped deadline.
- When organic egress alone meets `required`, fill drops to the heartbeat. When the cycle is
  `Recovered` the plan for it ends; the successor's plan starts when its clocks arm.

Hysteresis: emit a new rate only when it changes by more than 10 % or crosses the heartbeat, so
the action channel is not flooded during steady state.

### 1.3 Where each piece lives

| piece | crate / file | why there |
| --- | --- | --- |
| rate law, per-cycle state, tick scheduling | `transport/session/src/supervision/supervisor.rs` | pure state machine already owns `dims`, `largest_shares_seen`, the hard deadline and the tail |
| periodic tick, new action variant, coalescing | `transport/session/src/supervision/worker.rs`, `mod.rs` | the worker already runs the deadline timer and forwards actions |
| sending packets, SURB-reserve backoff, telemetry | `transport/session/src/manager.rs`, `utils.rs` | the Exit keep-alive stream and its `RateController` already exist (`manager.rs:3851`, `utils.rs:251`) |
| config, validation | `supervision/mod.rs` (`SupervisorConfig`, `validate_pix_supervision`), `manager.rs` (`validate_incoming_session_pix_config`) | same place as every other supervisor dial |
| operator config | `hoprd/src/config.rs` `UserIncomingSessionPixConfig` (hoprd repo) | the flattened mirror hoprd exposes |

Key reuse: `utils::spawn_keep_alive_stream` returns a `RateController`
(`balancer/rate_limiting.rs:17`) whose `set_rate_per_unit(n, period)` is exactly the knob the
driver needs. Each packet it sends already counts as one SURB consumed on the Exit and, on receipt,
one SURB consumed on the Entry (`manager.rs:4043`), which is what makes the Entry's balancer refill.
Nothing on the Entry side has to change for fill to work.

### 1.4 Rules

1. **Fill packets bypass the service gate**, as the existing notify keep-alives deliberately do
   (`manager.rs:3853` comment). Consequences: `served_total` stays organic-only; the predeposit
   budget and `max_served_without_progress` are untouched; the idle timer is refreshed by the
   progress fill produces, not by fill being counted as service.
2. **Only a funded front cycle in `Recovering` is filled.** Unfunded fronts, `AwaitingCommitment`,
   `AwaitingDeposit`, tombstones and closing Sessions get rate 0.
3. **Fill yields to organic traffic twice.** Once in the rate law (`required − organic`), and once
   at the sender: when the Exit's own SURB level estimate for the Session is below
   `min_surb_reserve`, the driver holds fill at the heartbeat so organic replies are never starved
   of SURBs.
4. **Fill is capped** by `max_rate`. Validation guarantees the cap is sufficient for the widest
   quota the node accepts (1.6), so the cap protects the Exit's ticket spend and the client's
   bandwidth without breaking the contract.
5. **Fill stops when the supervisor closes or the worker fails.** The driver sets rate 0 on every
   terminal path, before poisoning the gate.
6. **Nothing is negotiated on the wire.** `StartSession.additional_data` is full, and the contract
   is implied by `Capability::UsePIX`. Fill packets are ordinary keep-alives.

### 1.5 Wire format and compatibility

Fill packets are `HoprStartProtocol::KeepAlive` with the `BalancerState` flag and the Exit's SURB
level as `additional_data`, i.e. the same message the optional notify stream sends today. That is
deliberate: every shipped Entry already parses it, counts it as consumption and updates its view of
the Exit's buffer.

A dedicated `KeepAliveFlag::PixFill` bit would let the Entry meter fill precisely, but
`KeepAliveFlags::new(body[0])` (`protocols/start/src/lib.rs:827`) rejects unknown bits, so an old
Entry would drop the message and stop refilling. Sequence: first ship lenient decoding
(`new_truncated`) on the Entry, then introduce the flag one release later. Until then, Entry-side
metering is by inference (keep-alives received while the application read nothing).

### 1.6 Configuration and validation

New nested block on `SupervisorConfig`:

```rust
pub struct PixFillConfig {
    /// Default true.
    pub enabled: bool,
    /// Fill floor while organic egress covers the need. Default 60 s.
    pub heartbeat: Duration,
    /// Fraction of `max_recovery_time` by which the cycle should be complete. Default 0.75.
    pub finish_fraction: f64,
    /// Extra packets planned for loss. Default 0.05.
    pub loss_margin: f64,
    /// Fill ceiling in packets per second. Default 250 (about 2 Mbps).
    pub max_rate: u32,
    /// Re-planning interval. Default 1 s.
    pub sampling_interval: Duration,
    /// Window of the organic-rate estimate. Default 10 s.
    pub organic_window: Duration,
    /// Exit-side SURB level below which fill holds at the heartbeat. Default 500,
    /// matching `SurbStoreConfig::distress_threshold`.
    pub min_surb_reserve: u64,
}
```

Validation, in `validate_pix_supervision`: durations non-zero, `sampling_interval <= heartbeat`,
`0 < finish_fraction <= 1`, `0 <= loss_margin < 1`, `max_rate > 0`. In
`validate_incoming_session_pix_config`, next to the existing `max_recovery_time` check
(`manager.rs:978`): the cap must honour the contract at the widest accepted quota,

```text
max_rate >= ceil(quota_range.end() / PAYLOAD_SIZE × (1 + loss_margin) / (finish_fraction × max_recovery_time))
```

At the shipped defaults that is 127 packets/s against a cap of 250. Rejecting at load rather than
discovering one closed Session at a time is the same argument the existing check makes.

hoprd mirror (`UserIncomingSessionPixConfig`): expose `idle_fill_enabled`, `idle_fill_heartbeat`,
`idle_fill_max_rate`, `idle_fill_finish_fraction`; pin the rest to upstream defaults, named
explicitly as the file does for the other supervision fields. `hoprd-localcluster`'s `PixSettings`
gets the same four so the harnesses can switch fill off for the tests that pin the backstop.

### 1.7 Telemetry

| metric | type | labels |
| --- | --- | --- |
| `hopr_session_pix_fill_packets_total` | SimpleCounter, node-wide | none, so it survives hoprd's `/metrics` filter |
| `hopr_session_pix_fill_rate` | MultiGauge | `session_id` |
| `hopr_session_pix_fill_cycles_total` | SimpleCounter | cycles recovered while fill was above the heartbeat |

Add to `METRICS.md`. Existing `hopr_session_pix_closures_total{reason}` is what proves the backstop
still fires when fill cannot run.

### 1.8 Economics and abuse

- Ceiling on what fill can bill: one quota per `finish_fraction × max_recovery_time` per Session.
  The validator floors `max_recovery_time` at `quota_max / 1.5 Mbps`, so at a 649 MiB quota the
  fastest legal idle tariff is one quota per ~30 min.
- The Exit pays a relay ticket per fill packet. At the floor rate this is continuous but small, and
  inside the per-packet margin the Exit guide already requires.
- The Entry's only cap is `max_spend_per_window`; when it refuses, the Exit closes on
  `DepositTimeout`, which is the exhaustion ending hoprd-test already pins. A follow-up in
  edge-client should size that window against the tariff and surface it to the application.
- Fill delivers no application data, so the client's prepaid quota is consumed without goodput.
  This must be documented as the contract, not discovered.

## 2. Work breakdown

Six PRs, each independently mergeable and green. Conventional Commits titles as the repo's
`CLAUDE.md` asks; `cargo nextest run --lib` for unit tests, `-j 1` for integration tests.

### PR 1: lenient keep-alive flag decoding (`protocols/start`)

- `KeepAliveFlags::new(body[0])` → `new_truncated`, plus a reserved `PixFill = 0x04` bit that is
  parsed but unused.
- Test: a keep-alive with an unknown high bit decodes and is handled.
- Ships first so that every Entry in the field tolerates the flag before PR 6 starts setting it.

### PR 2: supervisor core (`transport/session/src/supervision`)

- `PixFillConfig` on `SupervisorConfig`, defaults, `validate_pix_supervision` rules.
- `supervisor.rs`: per-cycle `fill: Option<FillState { clock_start, last_served_total, organic_ema,
  last_rate }>`, armed together with the recovery clocks in `arm_recovery_clocks_for_earliest`
  (`supervisor.rs:432`); `pub fn next_fill_tick(&self) -> Option<Instant>`;
  `pub fn handle_fill_tick(&mut self, now, served_total) -> Vec<SessionPixAction>` implementing 1.2
  with hysteresis; rate reset to 0 in `close_ssa_and_collect`, `perform_recovered_transition` and
  on `closed`.
- `mod.rs`: `SessionPixAction::SetFillRate { packets: u32, per: Duration }`, coalescible like
  `ProgressNotification` (`worker.rs:334`); `next_deadline()` returns the minimum of the existing
  deadlines and the fill tick.
- Unit tests, pure and clock-driven like the existing ones (`supervisor.rs:1605` onward):
  - no fill before funding, none for an unfunded successor, none on a tombstone;
  - idle cycle ramps to `required`, finishes by `finish_fraction`, never exceeds `max_rate`;
  - organic egress at or above `required` holds fill at the heartbeat;
  - organic egress that stops mid-cycle raises fill to what is left, from the correct remainder;
  - predecessor tail is added to the remainder; loss margin is applied;
  - rate is 0 after `Recovered`, after every `Close`, and after `max_failed_cycles` retirement;
  - hysteresis suppresses sub-10 % changes;
  - validation rejects a cap below the widest-quota floor and accepts the defaults.

### PR 3: worker tick (`transport/session/src/supervision/worker.rs`)

- `worker_loop` waits on `min(next_deadline, next_fill_tick)`; on a tick it calls
  `handle_fill_tick(now, gate.served_total())` and dispatches.
- Tests mirroring the existing worker tests: a funded, idle supervisor produces `SetFillRate`
  actions at the sampling interval; the actions coalesce under a full channel; the final action on
  close carries rate 0; no ticks fire while nothing is funded.

### PR 4: sending and backoff (`transport/session/src/manager.rs`, `utils.rs`)

- In the Exit path of `handle_incoming_session_initiation` (`manager.rs:3851`), spawn the
  keep-alive stream for every PIX Session, not only when `surb_balance_notify_period` is set;
  keep `SurbNotificationMode::Level`. Store the `RateController` in the `SessionSlot` beside
  `pix_supervisor`.
- Driver (`spawn_pix_action_driver`, `manager.rs:2646`): on `SetFillRate`, apply
  `max(configured notify rate, fill rate)` unless the slot's SURB estimate is below
  `min_surb_reserve`, in which case apply the heartbeat; set rate 0 on every terminal path.
- Telemetry from 1.7; increment the packet counter in the keep-alive sink hook that already counts
  consumption (`manager.rs:3862`).
- Tests in the manager's test module, reusing `exit_session_originating_keep_alives`
  (`manager.rs:5376`): fill packets go out at the set rate; the Entry mock counts them as
  consumption; reserve backoff holds the rate; rate is 0 after close.

### PR 5: end-to-end behaviour (`transport/session/tests/pix.rs`, `hopr/hopr-lib/tests/transport_session_pix.rs`)

- `idle_session_is_completed_by_exit_fill`: same shape as `recovery_hard_deadline_closes_session`
  (`transport_session_pix.rs:829`), quota 100 000 and a 40 s deadline, but with fill enabled:
  fund the SSA, send nothing, assert `Recovered` and the successor's `NewDepositAddress` arrive
  before `finish_fraction × 40 s`, and the Session is still open afterwards.
- `recovery_hard_deadline_closes_session` explicitly sets `enabled: false`; it now proves the
  backstop with fill off. Add a sibling that leaves fill on but opens the Session **without** a
  SURB balancer and asserts the deadline still closes it: the guarantee is conditional on SURB
  supply, and this is the test that says so.
- `fill_yields_to_organic_traffic`: run the echo task at a rate above `required`; assert fill
  packets over the run are within the heartbeat's count plus a small margin, and echo throughput is
  unchanged against a fill-disabled baseline.
- `fill_resumes_after_organic_traffic_stops`: echo for a third of the cycle, stop, assert the
  cycle still completes inside the deadline.
- Multi-hop cases via the existing `#[case]` matrix.

### PR 6: documentation and downstream mirrors

- `supervision/mod.rs` header: a "Fill" section and a row per new parameter in the table; the
  worked 5 Mbps profile gains the fill numbers.
- `METRICS.md` rows for 1.7.
- RFC-0012 (`rfc` repo): a subsection after 2.3.7 "Expressed data quota" stating that the quota is
  a delivery commitment the Exit may fulfil with keep-alive packets, the rate law, and the
  conditions under which the guarantee holds; the reserved keep-alive flag.
- hoprd repo: `UserIncomingSessionPixConfig` fields and conversion, `PixSettings` in
  hoprd-localcluster, and a section in `EXIT_USER_GUIDE.md` titled "Idle clients and the minimum
  spend" with the tariff arithmetic. A localcluster test `session_pix_idle.rs` driving a
  balancer-enabled but silent Entry.
- edge-client repo (optional, after PR 1 has propagated): count `PixFill` keep-alives, expose them
  as `hopr_strategy_pix_fill_received_total`, and document sizing `max_spend_per_window` against
  the tariff.

## 3. Test matrix

| property | level | test |
| --- | --- | --- |
| rate law correctness, edge cases | unit, supervisor | PR 2 |
| tick scheduling and coalescing | unit, worker | PR 3 |
| packets leave at the set rate, backoff | unit, manager | PR 4 |
| idle cycle completes, successor funded | e2e, hopr-lib | PR 5 |
| backstop still closes without SURB supply | e2e, hopr-lib | PR 5 |
| fill yields under load | e2e, hopr-lib | PR 5 |
| production geometry, real chain, edgli Entry | manual, hoprd-test | scenario switch from app-layer filler to native |

## 4. Rollout and defaults

- Default `enabled: true`. This changes what an idle client pays, which is the point, and it is
  the safer default for Exits because it removes stranded revenue. Operators can disable it.
- Entry side needs nothing for fill to function. PR 1 is only a prerequisite for the metering flag.
- Two hoprd-test scenarios keep the app-layer filler as an oracle until the native one ships, then
  run both configurations and require identical outcomes.

## 5. Open questions for the hoprnet team

1. Should fill packets count as gated service after all? Ungated matches the existing keep-alive
   decision and keeps `served_total` organic-only; gated would let the predeposit budget bound fill
   on a not-yet-funded successor, which rule 2 already prevents.
2. Default `max_rate`: 250 packets/s is about 2 Mbps of fill; is that acceptable ticket spend for
   an Exit against an idle client, or should the default sit at the validator floor plus margin?
3. Whether the tariff period should become discoverable by the Entry, since it is the Exit's
   `max_recovery_time` today and the Entry cannot see it.
4. Whether to add an Entry-side `PixEvent` for fill so applications can show the idle cost, or
   leave that to metrics.
