# Surviving the loss of half the return relayers, with PIX

Parameters under which a PIX Session keeps its funded cycle — and the Exit keeps its deposit —
when one of its two return relays dies mid-cycle, and the derivation that picks them.

The scenario is `integration/tests/pix_relay_loss.rs`; the profile is
`integration/src/relay_loss.rs`.

```bash
HOPRD_SRC=../hoprd LINE=v5 just pix-relay-loss                                 # the three scenarios
HOPRD_PIX_ADDITIONAL_SHARES=16 HOPRD_SRC=../hoprd LINE=v5 just pix-relay-loss  # shipped surplus
HOPRD_SRC=../hoprd LINE=v5 just pix-relay-loss a_pix_cycle_survives_a_deep_return_relay_death_and_the_generation_wipe
```

`just pix-relay-loss` builds a PIX-enabled hoprd the way `just pix` does, starts **one chain**, and runs
each scenario in an invocation of its own on it: each kills a cluster node, so a cluster is never
shared. Manual, never in CI. Name scenarios to run only those. It forwards the environment:

| variable | meaning | default |
| --- | --- | --- |
| `HOPRD_PIX_ADDITIONAL_SHARES` | surplus shares per polynomial, clamped to `8..=255` | 32 |
| `HOPRD_PIX_POLYS` | polynomials per SSA (production: 4096) | 512 |
| `HOPRD_PIX_THRESHOLD` | shares that reconstruct a polynomial (production: 32) | 64 |
| `HOPRD_PIX_RATE` | offered packets per second, and so the Exit's egress | 150 |
| `HOPRD_PIX_SURB_BUFFER` | SURBs the Entry keeps buffered at the Exit | `16 × rate` |
| `HOPRD_PIX_MAX_SERVED` | `max_served_without_progress`, when the derived value is not wanted | derived |

The quota window, the fill ceiling and the recovery deadline follow the geometry, so a run at
production dimensions needs no recompile (§"Production dimensions").

## The answer in short

**Yes, and the measured set is below.** A PIX cycle survived losing one of its two return relays
mid-cycle in three of three runs, with every one of its 512 polynomials reconstructing and the
Exit's Safe rising by exactly one cycle's deposit each time.

Two things qualify that, and both are more interesting than the parameter list:

1. **The shipped surplus of 16 also survived.** At the SURB buffer depth a test cluster runs
   (2 400), the burst a relayer's death actually costs is smaller than even the shipped tolerance.
   The recommended 32 buys margin, not survival, *at this scale*.
2. **At a deployed buffer depth the margin is three shares.** The tolerance is `256 × surplus`.
   Written first as "no admissible surplus survives", on the assumption that upstream caps the
   surplus at the threshold (`256 × 64` = 16 384 shares against ~19 264 SURBs); hoprnet#8519 lifted
   that cap, production ships a surplus of 79 against a threshold of 32, and `256 × 79` = 20 224.
   A deployed Exit's whole buffer, which one generation bump discards, costs each polynomial 76 of
   those 79 shares. §"Production dimensions" has the runs that measured it.

So the durable part of this answer is the **inequality**, not the number. It held again on the stack
of hoprnet#8506 and #8576 (§"Results on the dead-verdict stack"), where the wipe the inequality
prices became deterministic.

## The question, and why it is not the one `return_path.rs` asks

`tests/return_path.rs` kills a return relay and asks whether the *stream* survives. It is
satisfied by the stream degrading and recovering, because a byte that arrives late is still a byte
delivered.

PIX is not satisfied by that. A cycle pays only if **every** polynomial reconstructs — the SSA is
the sum of all 512 constant terms — and the deposit's address derives from the Entry's and the
Exit's commitments together, so a cycle one share short of one polynomial strands money that has
already been paid. There is no partial credit and no refund. The relevant question is therefore
not "did the stream recover" but "did every polynomial clear its threshold".

## Why the documented loss tolerance is the wrong instrument

`additional_shares` is documented against a loss *rate*: a polynomial reconstructs from the first
`threshold` distinct shares out of `threshold + surplus` emitted, so it survives

```
p  ≤  surplus / (threshold + surplus)
```

— 20 % at the shipped 64 + 16, not 25 %. It is worth being explicit that **a surplus of 33 against
a threshold of 64 does not buy survival of a 50 % loss**: it buys 33/97 = 34 %. Surviving a
sustained 50 % needs `surplus = threshold`, which is the largest value upstream accepts
(`validate_pix_dimension_product` refuses more, since insurance above the payload it insures is a
misconfiguration), and even then every one of 512 polynomials would have to independently clear a
coin-flip margin — the probability of all of them doing so is nil.

But a relayer's death does not produce a sustained 50 % loss. It produces **one burst**, and then
the Entry routes around it. The burst is what has to be sized against.

## What the burst is made of

Entry `E`, relays `R` (which dies) and `S`, Exit `X`. Return paths are drawn independently per
SURB, so each relay carries about half the return stream.

1. **SURBs already at the Exit that name `R`.** A share is bound to its SURB at mint time, and
   reaches the reconstructor only when the first relayer acknowledges the packet carrying it
   (`create_surb_for_path` encrypts the share under the first-relayer challenge solution). Spending
   one through a dead `R` reveals nothing. About half the Exit's buffer is in this state the
   instant `R` dies.

2. **The other half of the Exit's buffer.** This is the term that does not follow from anything PIX
   documents, and it is the largest one. The Entry notices the silence, re-plans, and bumps its
   SURB **generation**; `SurbRingBuffer::push` then clears **both tiers wholesale** on the first
   batch carrying the newer generation. Its comment reads "everything held is for the superseded
   path", which is true when a return path is a property of the Session and false here: paths are
   drawn per SURB, so roughly half the buffer named the surviving `S` and would have delivered.
   Those shares go too, and they are not counted in `evicted_shares`, which counts capacity
   overflow only — so nothing reports them.

3. **Everything spent into the void during detection.** The detector needs five consecutive
   one-second flushes with no reply over a pair of legs, corroborated by a sibling pair that *is*
   delivering (`SILENT_FLUSHES_BEFORE_DEGRADED`, `MIN_EXPECTED_FOR_SILENCE` in
   `transport/hopr/src/protocol/surb_telemetry.rs`). Until it fires, about half of what the Exit
   sends is addressed to a corpse.

With `B` the Exit's SURB holding, `R_e` its egress rate and `T_d` the detection window:

```
burst  ≈  B  +  ½ · R_e · T_d
```

## What absorbs a burst

Emission is **round-robin over `SHARE_EMISSION_WINDOW` = 256 polynomials advancing in lockstep**.
That is not incidental — the constant exists for exactly this, and its doc comment says so: a
contiguous run of lost SURBs "would otherwise land on a single polynomial", and round-robin
"spreads the same run across the window".

So a burst of `L` costs each polynomial in the window `L / 256`, and each can afford `surplus`:

```
tolerated burst  =  256 · surplus
```

**The design rule is therefore `256 · surplus ≥ margin · (B + ½ · R_e · T_d)`.** `surplus` is sized
against the Exit's *SURB buffer depth*, not against a packet-loss percentage. `relay_loss::burst_margin`
is that ratio, and the profile is chosen to clear 2.0.

## The second parameter, which is not a PIX one

Surviving the burst is necessary and not sufficient. After the wipe the Exit holds nothing and the
Entry does not know it: the balancer estimates the counterparty's buffer as `produced − consumed`,
and consumption is only observed when a reply gets home, so a return path dropping everything reads
exactly like a well-stocked counterparty. hoprnet#8490 pins that in a unit test — the estimate sits
at or above target and the controller winds production to zero, precisely while the Exit empties.

`SurbBalancerConfig::sustain_on_return_path_loss` is the opt-in that breaks the deadlock, and it is
**off by default and off in every client shipping today**. It takes effect only once an outside
observer marks the path degraded — which the same re-plan that bumped the generation does — and
then the balancer drops to open loop and produces flat out for the 10 s grace
(`RETURN_PATH_DEGRADED_GRACE`, a private constant in `hopr-transport`). `max_surbs_per_sec` has to
replace a whole buffer inside that grace, which is what `4 × R` against a `16 × R` buffer buys.

Without it, a cycle that survives the burst starves immediately afterwards — a different failure
with the same symptom.

**That opt-in is gone from this suite.** It is a client setting (GNO-713), hoprnet#8506 removes the
field, and the suite no longer names it, so that it builds on both sides of that change. What does
its job now: #8506 writes off the SURBs through a relayer judged dead, and hoprnet#8576 resets the
balancer when the Entry advances the SURB generation (the level estimate restarts from empty, a
pre-wipe report from the Exit is ignored while the refill is paced) and paces the refill at
`target / 5` SURBs per second for 5 s instead of the PID's ceiling. The 2026-10-27 results below
were taken on a prototype of that. On a stack with neither, the starvation described above is back,
and it shows up as `recovery_idle` rather than as a stream stall.

## The parameters

| knob | value | why |
| --- | --- | --- |
| `num_ssa_parts` | 512 | a whole number of 256-wide emission windows, so a burst is always spread over the full 256 |
| `ssa_part_size` | 64 | the deployed threshold, unchanged |
| **`additional_shares`** | **32** | `256 × 32` = 8192 tolerated against ~3000 expected — a margin of 2.7 |
| `E` | 49 152 packets | 328 s nominal at 150 pkt/s |
| quota | 71.4 MB | `E × PIX_QUOTA_BYTES_PER_SHARE` |
| `quota_range` | 36 864 … 65 536 shares | holds the whole `8..=64` surplus sweep, so the control run faces one admission policy |
| `target_surb_buffer_size` | 2 400 (`16 × R`) | 4.9 % of `E`, the deployed ratio — and the dominant term in the burst |
| `max_surbs_per_sec` | 600 (`4 × R`) | the mint ceiling; the old open-loop refill had to replace a whole buffer inside the 10 s degraded grace with it |
| `max_recovery_time` | 900 s | `shapes`' value; also what bounds how long a *failure* takes to surface |
| `fill_finish_fraction` | 0.6 | aim point 540 s |
| `fill_max_rate` | 250 | clears the 128 pkt/s floor the widest accepted quota implies |
| `max_served_without_progress` | 2048 | free credit `512 × 32` = 16 384 covers the 2 400 queue |

The surplus is billed on purchase — it rides in the negotiated `PixParams` and the per-SSA quota
counts it — so 32 rather than 16 makes every deposit 20 % larger. That is the price of the
insurance, and it is charged whether or not a relay ever dies.

### What is *not* recommended

**Shrinking the Exit's SURB buffer**, even though it is the cheapest way to satisfy the
inequality: `B` is the dominant term, so halving it halves the burst. It is also the Exit's entire
runway, and a buffer sized for the failure rather than for the service measures a Session nobody
would deploy. The depth stays at the deployed ratio and the surplus pays for the failure.

## What the scenario does

Three scenarios, each on a cluster of its own. **Symmetric** and **return-only** use an Exit and
**two** relay candidates (three nodes), so killing one is killing half: one hop each way, or direct
requests and relayed replies. **Return-two-hop** uses three relay candidates (four nodes), direct
requests and two-hop return paths over them, so the SURBs with the victim at hop two are ones the
Exit cannot recognise as dead and only the Entry's invalidation removes them. PIX derives the share
key from the first *return* relayer's acknowledgement, so a zero-hop return path carries no share at
all; the forward path may be direct. Traffic is offered at 150 pkt/s against the Exit's loopback,
so one chunk offered is one reply packet and therefore one share.

1. Wait until a **funded** cycle is 25 % through — unambiguously under way, well clear of the 85 %
   early-recovery signal, so no successor exists and the cycle under test is the only one the
   Session has.
2. Check **both** relays carried ≥ 30 % of the forwarded traffic. This is the non-vacuity guard the
   scenario cannot do without: against an already-concentrated stream, killing the busiest removes
   either everything or nothing, and either way says nothing about losing *half*.
3. SIGKILL the busiest. SIGKILL rather than a clean stop: a crashed relay announces nothing and
   closes no channel, so the invalidation path keyed on channel closure never fires.
4. Keep offering traffic, and wait for the Exit to sweep.

It then asserts, in order of what each rules out:

- **No supervisor closure at all** (`hopr_pix_closures_total`). This is the one that names the
  cause: a starved cycle produces `recovery_idle`, roughly a minute after the Entry runs out of
  shares to emit for a cycle it can no longer complete.
- **No deposit timed out** — separates a cycle lost to the outage from one lost to a slow chain.
- **A cycle was swept** — every one of the 512 polynomials reached threshold despite the burst.
- **The Exit's Safe rose by at least one cycle's deposit.** The counters alone cannot establish
  this: `keys_recovered` and `sweeps` both increment on the "already swept" path, where a key is
  recovered against a stealth address the deposit never reached.

## Results on the stack of 2026-10-17

Before hoprnet#8506. The dead-verdict stack's results follow the table of every run.

Measured 2026-10-17 on a 3-node local cluster, binary chain. Entry is in-process `edgli` on
hopr-lib `1462e4a0`; the Exit and relays are a `hoprd` release build (hoprd `227e0c2`, hopr-lib
`6c66dddd` — one chore commit apart, no protocol difference).

### Symmetric, surplus 32 — **passed** (464 s)

`a_pix_cycle_survives_a_relay_death_on_both_paths`. Killed at **25.8 %** of the cycle (12 663
shares in), victim carrying **60.8 %** of the forwarded traffic, so the survivor had 39.2 % — a
genuine two-way split rather than a stream that had already concentrated.

| reading | value |
| --- | --- |
| `hopr_pix_shares_total{useful}` | **+32 768** = exactly `512 × 64` — every polynomial reconstructed |
| `hopr_pix_shares_total{surplus}` | +7 569 |
| `hopr_pix_gate_blocks_total{share_lag}` | 0 episodes |
| `hopr_pix_closures_total` | absent — the supervisor never closed the Session |
| cycles | 1 requested, 1 committed, 1 funded, 1 recovered, swept and paid |
| foreign Session steps | 0 / 162 samples |

The outage reads as a **two-second** hole in the trace (`U12672/96s` `.2s` `U2232/30s` …), and the
per-second SURB round-trip counters show the stream collapsing to zero at the kill and climbing
back to ~70/s within ~18 s, against ~130/s before — half the capacity, one relay.

**The detector never fired, and that is the finding.** No `return path silent, re-planned` line
appears anywhere in the run. With one hop in *each* direction the kill silences the forward path
at the same instant, so every leg goes quiet together — and `degraded_destinations` refuses to act
without a sibling pair to the same destination still delivering. The Session recovered on the path
cache's own re-weighting (10 s TTL / 5 s refresh) instead.

So this shape never reaches the generation bump, and **the burst it inflicts is the small one** —
the SURBs already bound for the dead relay, and a couple of seconds of spend. The cycle would very
likely have survived on the shipped surplus too. Which is exactly why the second shape exists.

### Return-only, surplus 32 — **passed** (443 s)

`a_pix_cycle_survives_a_return_relay_death_with_the_forward_path_intact` — 0-hop forward, so the
forward path is untouched by the kill, one reply leg keeps delivering, the detector corroborates,
and the generation bump discards the Exit's whole buffer. This is the burst the surplus is sized
against, and this run reached it.

Killed at **25.7 %** of the cycle (12 624 shares in, 95 s), split **53 / 47** — the asymmetric
topology makes the histogram pure return traffic, so that is the split of the return stream and
nothing else.

| reading | value |
| --- | --- |
| `hopr_pix_shares_total{useful}` | **+32 768** = exactly `512 × 64` |
| `hopr_pix_shares_total{surplus}` | +6 638 |
| `hopr_pix_gate_blocks_total{share_lag}` | absent — the gate never parked |
| `hopr_pix_closures_total` | `[]` — the supervisor never closed the Session |
| cycles | 1 recovered, **2** deposits confirmed (the successor was funded) |
| Exit Safe delta | **0.071368704 wxHOPR** = exactly one cycle's deposit |
| echo arrival | 97.7 % of 35.8 MB offered |

**The whole mechanism is legible in the per-second SURB round-trip counters:**

```
20:40:13  kill            expected=109 observed=93     ← normal
20:40:14  …               expected=75  observed=72     ← half the replies now go nowhere
20:40:18  …               expected=65  observed=71
20:40:18.886  "return path silent, re-planned  entries=1"
20:40:18.886  "refilling behind the re-plan    sessions=1"
20:40:19  refill          expected=601 observed=115    ← open loop: produce flat out
20:40:22  …               expected=741 observed=130    ← replies back at full rate
20:40:33  grace lapses    expected=258 observed=128
20:40:34  closed loop     expected=21  observed=127
```

`expected` is SURBs minted per second. The jump from ~65 to ~700 is
`sustain_on_return_path_loss` doing exactly what it is for: the balancer drops its (now
meaningless) counterparty estimate, produces at the ceiling for the 10 s grace, and replaces the
buffer the generation bump discarded. Detection to full recovery is **~6 s**, and unlike the
symmetric shape the rate comes back to the full ~130/s rather than half of it — only the return
direction lost a relay.

### The two controls — **both passed**

| | surplus 16, sustain **on** | surplus 16, sustain **off** (fully shipped) |
| --- | --- | --- |
| wall | 432 s | 430 s |
| killed at | 25.8 % (10 582 / 40 960) | 26.1 % (10 703 / 40 960) |
| split before the kill | 51 / 49 | 50 / 50 |
| `shares{useful}` | **+32 768** | **+32 768** |
| `shares{surplus}` | +4 095 | +4 206 |
| `longest_surplus_run` | 1 047 | 1 329 |
| `gate_blocks{share_lag}` | absent | absent |
| `closures` | `[]` | `[]` |
| Exit Safe delta | 0.05947392 wxHOPR = 1 cycle | 0.05947392 wxHOPR = 1 cycle |
| detector fired | no | no |

**Neither reached the wipe**, so neither is evidence that surplus 16 survives a generation bump —
only that the bump did not happen, which turns out to be the common case. With no bump,
`sustain_on_return_path_loss` is inert by construction (it needs a degraded-path mark to take
effect), which is why the two controls are indistinguishable.

What the shipped run does show is the **slow** recovery path, and it is worth the contrast:

```
            detector fired (surplus 32)      detector did not (shipped)
kill        observed 93 → 65/s               observed 95 → 65/s
+6 s        back to ~130/s                   still ~65/s
+16 s       —                                still ~65/s
+19 s       —                                legs=1, climbing (~88/s)
+25 s       —                                back to ~127/s
```

Without the detector, nothing supersedes anything: the Entry simply stops drawing the dead relay
once the path cache re-weights it out (5 s refresh / 10 s TTL), and the Session bleeds ~50 % of
its replies for **~25 s** instead of ~6 s. Roughly 1 600 shares, gradually — comfortably inside
even the shipped 4 096.

### Why the wipe is occasional rather than systematic

(On the stack of 2026-10-17, where the detector was the Entry's silence detector. Dead verdicts
from SURB reply races replace it in hoprnet#8506, and the wipe follows each one; see §"Results on
the dead-verdict stack".)

The detector fired in one of the three runs. Chasing why is the most useful thing these runs
produced, because it bounds how bad the problem actually is.

`degraded_destinations` only counts a flush against a pair of legs that minted at least
`MIN_EXPECTED_FOR_SILENCE` (20) SURBs in it, and it needs five consecutive such flushes. But the
Entry stops minting over a dead relay on its own: the path cache re-weights every 5 s from graph
scores, and an edge whose ack rate has collapsed falls below `min_ack_rate` and leaves the
candidate set. So detection is a **race between the graph scoring the relay down — which stops the
minting and therefore hides the evidence — and the five-flush counter**. Whichever wins is a matter
of a second or two.

The consequence cuts both ways. The wipe is rarer than the model assumes, so a deployed Session is
less exposed than the arithmetic alone suggests. But it is also *unpredictable*, and when it does
fire it discards the whole buffer at once — so it cannot be designed around by hoping.

## What these runs establish, and what they do not

**Established.** A PIX cycle survives losing one of two return relayers, mid-cycle, at this
geometry — in three of three runs, across both topologies and both surpluses, with every one of
512 polynomials reconstructing (`useful` landed on exactly 32 768 every time), no egress-gate
stall, no supervisor closure, and the Exit's Safe rising by exactly one cycle's deposit. That is
the headline answer and it is not marginal.

**Established.** The recovery mechanism works and is observable: detection to full throughput in
~6 s, with `sustain_on_return_path_loss` visibly refilling the wiped buffer at ~700 SURBs/s.

**Not established: that surplus 32 is necessary.** The one run that reached the wipe was at
surplus 32, and the one at surplus 16 never faced it. At this buffer depth the measured burst sits
under even the shipped tolerance of 4 096, so 32 buys margin rather than survival. Settling this
needs a way to force the detector to fire — the honest next experiment, and not one of these.

**Derived, then measured: the rule scales, with three shares to spare.** These runs were first
read as "the rule does not scale": the tolerance is `256 × surplus`, upstream was taken to cap the
surplus at the threshold (`256 × 64` = 16 384 shares), and a deployed Exit holds ~19 264 SURBs
(`docs/pix-traffic-shapes.md`), all of which a generation bump discards. hoprnet#8519 lifted that
cap. Production ships a surplus of 79 against a threshold of 32, which tolerates `256 × 79` =
20 224 shares, and the deployed buffer's wipe costs each polynomial 76 of the 79:
`relay_loss::tests::the_deployed_buffer_is_covered_by_the_production_surplus_with_three_shares_to_spare`
states it against the same constants, and §"Production dimensions" measures it.

### Every run, in one table

| # | topology | surplus | sustain | detector fired | wall | `useful` | Safe delta | result |
| - | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | symmetric | 32 | on | no | 464 s | 32 768 | 1 cycle | **pass** |
| 2 | return-only | 32 | on | **yes** | 443 s | 32 768 | 1 cycle | **pass** |
| 3 | return-only | 16 | on | no | 432 s | 32 768 | 1 cycle | **pass** |
| 4 | return-only | 16 | off | no | 430 s | 32 768 | 1 cycle | **pass** |

`useful` is `hopr_pix_shares_total{kind="useful"}` over the scenario; `512 × 64` = 32 768 is a
complete cycle and every run landed on it exactly. No run recorded an egress-gate block, a
supervisor closure, a deposit timeout or a foreign Session in its trace.

## Results on the dead-verdict stack (2026-10-27)

**Measured on a prototype of hoprnet#8576, not on its final head.** The prototype advances the SURB
generation when a dead verdict first reaches a session and writes off the session's whole stock, on
top of hoprnet#8506 (graph health from SURB reply races, FIFO pop order on the Exit). The final head
differs from it: a count-only write-off, a one-shot balancer reset and a paced refill, a re-bump
cooldown, race verdicts that follow the pop order, and a gate on every finite
`max_surbs_per_data_packet`. **The matrix on the final head follows and replaces these figures.**

One chain and one cluster per run, FIFO pop order, hoprd `main` with the localcluster link faults
(hoprnet/hoprd#248), edgli `7e1ef15`, the 512 × (64 + surplus) profile at 150 pkt/s with a 2 400 SURB
buffer, the victim killed at about 26 % of a funded cycle.

### Does a cycle survive the wipe?

| scenario | surplus | tolerated burst (`256 × surplus`) | wipe after the kill | SURBs wiped | arrival | wall | result |
| --- | --- | --- | --- | --- | --- | --- | --- |
| return-two-hop | 32 | 8192 | +2 s | 2403 | 95.6 % | 514 s | pass: swept, Safe +1 cycle |
| return-two-hop | 16 | 4096 | none (no verdict this run) | — | 91.9 % | 500 s | pass: swept, Safe +1 cycle |
| return-two-hop | **8** | 2048 | +0 s | 2508 | 28.6 % | 1116 s | **fail**: closed as `recovery_idle` |
| return-only | 32 | 8192 | +1 s | 2412 | 96.1 % | 456 s | pass: swept, Safe +1 cycle |
| symmetric | 32 | 8192 | +5 s | 2387 | 99.2 % | 448 s | pass: swept, Safe +1 cycle |

The wipe is now deterministic (it fired within 5 s in four of the five runs; the earlier silence
detector fired in one of four) and costs exactly the Exit's stock, about 2 400 shares here. A cycle
survives it iff `256 × surplus` covers that stock plus the detection burst: 8192 and 4096 do. The
surplus-8 run is the **negative control** and fails as predicted: 2508 wiped against 2048 tolerated,
and the cycle starved. The surplus-16 run reached no verdict and survived without a wipe: for a deep
victim the first hop still acknowledges, so shares arrive and only replies are lost.

### Production dimensions

4096 polynomials × (32 + 79), a cycle of 454 656 packets and a quota of 660 160 512 B (0.660160512
wxHOPR). The environment of these runs:

```bash
HOPRD_PIX_POLYS=4096 HOPRD_PIX_THRESHOLD=32 HOPRD_PIX_ADDITIONAL_SHARES=79 HOPRD_PIX_RATE=400 \
HOPRD_PIX_SURB_BUFFER=7000 \
  HOPRD_SRC=../hoprd LINE=v5 just pix-relay-loss a_pix_cycle_survives_a_deep_return_relay_death_and_the_generation_wipe
# and HOPRD_PIX_SURB_BUFFER=19264 for the deployed worst case
```

The rate is raised from production's so a run takes about 25 minutes: the nominal cycle is 1136 s at
400 pkt/s. `max_recovery_time` is 2272 s (twice that, production ships 3600 s) and the fill ceiling
is about 350 packets/s, what the Exit's own validator demands of this geometry (production's 250
covers its cycle inside its 3600 s). Two fixes to the profile were needed to run at all:

1. **The fill ceiling is derived from the Exit's validator.** Upstream's 250 packets/s cannot finish
   the widest accepted quota inside `max_recovery_time × fill_finish_fraction` at this size, and hoprd
   refuses the configuration at start and never comes up (`relay_loss::fill_max_rate`).
2. **The warm-up's "funded" condition is absolute, not a delta.** At this size the deposit confirms
   about 2 s after the Session opens, before the baseline sample, so a delta waits for the *successor*
   cycle's funding at 85 %. The first run's kill, meant for 25 %, landed at 83.5 %.

| run | SURB buffer | kill at | verdict and wipe | arrival | wall | result |
| --- | --- | --- | --- | --- | --- | --- |
| RP7000, first (late kill) | 7000 | 83.5 % | none in the 3 min left | 97.9 % | | pass: swept; survived on FIFO alone, the stock turns over in about 18 s |
| RP7000b | 7000 | 25.0 % (113 839 shares) | verdict +1 s, wipe +2 s of 7246 SURBs; re-verdicts at +91 s and +166 s (754 and 802) | 96.6 % | 1450 s | pass: swept, Safe +0.660160512 wxHOPR |
| RP19264 | 19 264 | 25.0 % (113 798 shares) | verdict +1 s, wipe +1 s of 19 441 SURBs; re-verdicts at +131 s and +215 s (1828 and 1226) | 96.3 % | 1405 s | pass: swept, Safe +0.660160512 wxHOPR |

- **7000 SURBs (the client's target):** the wipe costs each polynomial about 27 of its 79 surplus
  shares, a margin of 52. All 4096 polynomials reached threshold; 286 936 of 323 584 emitted surplus
  shares were accepted, about 9 lost per polynomial on average to the three wipes and the replies lost
  before the first verdict.
- **19 264 SURBs (the deployed worst case):** the first wipe alone costs about 76 of the 79, a margin
  of **three**. It survived because detection took 1 s, where the profile's burst model budgets 8
  (`expected_burst` 20 864 against a tolerated 20 224), and because the later wipes landed in later
  emission windows. This is the case for keeping the Exit's stock near the target rather than its
  capacity, and for not wiping the same stock twice (hoprnet#8576's gate on `max_surbs_per_data_packet`
  and its re-bump cooldown).
- **The re-verdicts:** the verdict lapses 60 s after the last SURB through the relayer was minted, the
  planner's 10 % exploration draws it again, and the new verdict wipes again. The prototype did not
  stop that; the final head's cooldown does.
- No SURB store overflow and no `ring_share` eviction on the Exit in any of these runs (read from the
  node logs then; the scenario now asserts it from the metrics).

## Fidelity limits

- **Transit is a memcpy.** All three nodes are on localhost, so a shallower buffer suffices than a
  deployment needs and the detection window is at its floor. The *ratio* the profile is built on
  transfers; the absolute buffer depth does not.
- **Two relays is the sharpest case, not the common one.** With four candidates a death costs a
  quarter of the stream and the planner has three places to go. Two was chosen because the survivor
  has to carry everything.
- **One client, one cycle.** Nothing here measures an Exit serving several Sessions through the
  same dying relay, which is where the wipe would land on several cycles at once.
- **The burst is an estimate.** `DETECTION_SECS` sums three upstream constants that are not
  configurable and not individually observable from the test. That is why the profile is sized to a
  margin rather than to the figure, and why the run reports the shares actually lost.
