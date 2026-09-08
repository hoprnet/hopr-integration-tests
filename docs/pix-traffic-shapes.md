# PIX under end-user traffic shapes

What a PIX Session does under the traffic a VPN client actually produces, measured rather than
derived. The scenarios are `integration/tests/pix_shapes.rs`; the profile they run at is
`integration/src/shapes.rs`.

Run them with:

```bash
HOPRD_SRC=../hoprd-shapes just pix-shapes            # all of them, hours
HOPRD_SRC=../hoprd-shapes just pix-shapes <scenario> # one, ~12-20 min
```

`HOPRD_SRC` must be a hoprd carrying **hoprnet#8396** (Exit-side PIX fill) and the
`--pix-config` seam. `just pix-shapes` refuses to run without the latter, because a
`hoprd-localcluster` that ignores `--pix-config` silently runs the demo geometry and every
assertion below would then be measuring a cycle 2 500x smaller than it claims.

## The profile

```
parts 1024 x (threshold 64 + surplus 16)   E     = 81 920 packets/cycle
R     = 300 packets/s  (~2.5 Mbps)         cycle = 273 s nominal
quota = 81 920 x 1038                            = 85 032 960 B (81.1 MiB)
```

**Not the deployed geometry, and deliberately so.** What governs PIX is ratios, not rates: the
client's SURB buffer is `16 x R` and a cycle is `cycle_seconds x R`, so `buffer / E = 16 /
cycle_seconds` and the packet rate cancels. A buffer that is a realistic fraction of a cycle
therefore depends on the cycle *length* alone, and a ~4.5 min cycle reproduces the deployed 5.2 %
at a rate a 1-hop local cluster carries comfortably. The deployed 4608 x 80 at 10 Mbps would be
10-15 min per cycle and put a full pass out of reach.

| quantity | value | deployed |
| --- | --- | --- |
| SURB buffer | 4 800 SURBs (5.9 % of `E`) | 19 264 (5.2 %) |
| free credit (`parts x surplus`) | 16 384 | 73 728 |
| `max_served_without_progress` | 2048 (credit covers the queue) | 2048 |
| `max_recovery_time` | **12 min** | 2 h |
| fill rate an idle cycle needs | 120 packets/s | 72 packets/s |
| `fill.max_rate` | 250 | 250 |

`max_recovery_time` is the one deliberate departure. It is no longer only a backstop: since the
Exit fills a cycle the application left unfinished, it is also the idle tariff, and an idle
scenario spends `0.75 x` it. Two hours is not a thing a test can wait out. The value still clears
the floor `validate_incoming_session_pix_config` enforces (`quota_range_max / 180 packets/s` =
535 s), so it is a legal configuration rather than a test-only escape hatch.

## Results

Measured on a 3-node local cluster, 1 hop, binary chain. Every figure is from a run whose log is
named beside it.

All six pass. Counters are the Exit's `hopr_strategy_pix_*` delta over the scenario.

| shape | traffic offered | recovered | swept | deposits | wall |
| --- | --- | --- | --- | --- | --- |
| **geometry spike** | 88 MB at 300 pkt/s, loopback | 1 | 1 | 2 | 590 s |
| **idle** | 25 keep-alives of 32 B, one per 25 s | 1 | 1 | 2 | 791 s |
| **browsing** | 40 kB bursts every 3 s (~13 kB/s), 540 s | 1 | 1 | 2 | 708 s |
| **download** | service pushes 2 cycles at 300 pkt/s | 2 | 2 | 3 | 715 s |
| **upload** | 300 pkt/s into a sink, nothing returns | 1 | 1 | 2 | 690 s |
| **mixed** | browse 180 s → quiet 120 s → bulk 1 cycle | 2 | 2 | 3 | 1112 s |

Not one scenario recorded a deposit timeout, and none was closed by the supervisor.

Traffic that had to round-trip did: 100 % arrival on the spike, on the idle liveness echo
(7 200/7 200 B) and on the mixed browsing phase; 99.9 % on the mixed bulk phase. The upload's
sink absorbed everything offered and returned nothing, which is the shape.

**The two shapes that carry no return traffic of their own — idle and upload — completed their
cycles and funded their successors anyway.** That is Exit-side fill (hoprnet#8396) working end to
end through a real chain, and it is the result this whole exercise was for: before it, both of
those clients stranded every deposit they made while paying for the quota.

### What the numbers say

**The achieved packet rate is ~232/s against 300 offered** (0.21 Mbps of 0.27), so an achieved
cycle runs about 1.3x nominal rather than the 2-3x the deployed profile's docs predict. The gap is
the local cluster's near-zero transit: the SURB pipeline that stretches a deployed cycle is mostly
latency, and there is almost none here. **This is the profile's main fidelity limit** — see below.

**Browsing is much closer to idle than to busy.** 40 kB every 3 s is ~13 kB/s, which is ~14 return
packets/s against the ~114/s a cycle of this geometry needs inside its deadline. So a browsing
client's cycles are completed by *fill*, not by the browsing. The first version of that scenario
asked for two cycles' worth of packets at that rate — a payload nearly four hours long — and
stopped 4 % in.

**`max_served_without_progress` stays at upstream's 2048.** The free credit (16 384) covers the
queue (4 800) by more than 3x, so the drain after a recovery is credited as liveness and the gate
never blocks part-way through it. This contradicts `hoprd/EXIT_USER_GUIDE.md` §7 and
`PIX-TUNING.md` §7, which both still say to raise it to 20 000; hoprnet's current supervisor
documents the flat 2048 as safe at any dimensions because `RecoveryProgress` now follows
`shares_seen` rather than `useful_shares`. Nothing here needed it raised. **Those two documents
should be corrected.**

**A download completes cycles about twice as fast as anything else**, and for the expected reason:
it is the only shape whose application traffic saturates the direction PIX bills, so it swept two
cycles in the time the others took to sweep one. Fill has nothing to make up on it.

## Three findings that are not about PIX

**The entry closed its own channels ten minutes in.** `close_below_quality_score` defaults to 0.3
and is separate from the eligibility threshold this harness already zeroes. Probing needs traffic
to score a peer; an idle shape offers 32 bytes every 25 s; the score decays below the threshold and
the strategy closes the channel underneath a Session that is working perfectly. Measured: channels
opened at T+0, closed at T+10m07s, then 17 522 consecutive `cannot find 1 hop path` failures. Any
scenario longer than about ten minutes hits it. `env.rs` now pins it to 0.0.

For a deployment this is worth knowing rather than alarming — a real network has more relays and
more probing traffic — but an idle VPN client is exactly the case that produces neither, and the
interaction between a quiet Session and quality-scored channel closure is real.

**The echo pump cannot express an asymmetric shape.** `pump_halves`' reader decides completion
against the payload that was *sent*, because every other scenario here targets the Exit's loopback.
A download's reply volume is the service's choice, so the pump declared `Complete` after 0.23 s
against a 491 s push; an upload's is zero, so it would have declared `NeverStarted` at 30 s. Both
shapes now drive the Session directly.

**Reading a Session whose peer has stopped sending burns CPU.** The download's drain originally ran
out its full budget rather than stopping when the sweeps landed, and spent two hours reading a
Session whose service had finished pushing — four CPU-hours in thirty wall-clock minutes, roughly
seven cores. The scenarios now stop their traffic as soon as the cycles they were waiting for have
completed, which is correct regardless; but a read loop on an idle Session costing that much is
worth someone looking at upstream, since a real client does exactly that between transfers.

## Fidelity limits

- **Transit is a memcpy.** All three nodes are on localhost, so the round trip is sub-millisecond
  and a shallower SURB buffer suffices than a deployment needs. The absolute buffer depth here does
  not transfer; the *ratio* is what these runs are about. `cluster::request_latency_profile()` can
  simulate a WAN RTT and a run that cares should say what it simulated.
- **One client.** Nothing here measures an Exit serving several Sessions at once, which is where
  `max_live_cycle_bytes` and the uplink start to bind.
- **The price is scaled.** 1e-9 wxHOPR/B rather than the deployed 5.33e-8, because the cluster
  funds each node's Safe from a fixed pot. Nothing in the protocol reads the absolute figure — both
  sides check the *product* with the quota — so the exchange measured is the same one.
