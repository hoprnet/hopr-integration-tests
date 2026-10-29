# Return-path resilience — scenario catalogue

Scenarios for the weaknesses behind the 2026-08-11 `gnosis_vpn-client` return-path break,
and the fixes for them: [hoprnet#8328] (SURB pop order), [hoprnet#8329] (invalidate
relayers whose channel closed), [hoprnet#8331] (return-path diversity), [hoprnet#8330]
(frame max-age), [hoprnet#8341] (report SURB round-trips into the network graph) and
[hoprnet#8345] (act on return-path silence from the supply side).

The last two are what turn the earlier fixes from "a better return path is available" into
"a broken one is noticed and replaced": #8341 gives the graph per-leg round-trip evidence,
and #8345 acts on sustained silence. They are the mechanism
`session_should_survive_return_relayer_loss` measures.

Section references (§) are to the [HOPR protocol summary][summary] at the commit the
`hopr-debug` skill pins. They are the reason each scenario is shaped the way it is —
several obvious-looking designs measure nothing, for reasons that only the protocol
explains.

[hoprnet#8328]: https://github.com/hoprnet/hoprnet/pull/8328
[hoprnet#8329]: https://github.com/hoprnet/hoprnet/pull/8329
[hoprnet#8330]: https://github.com/hoprnet/hoprnet/pull/8330
[hoprnet#8331]: https://github.com/hoprnet/hoprnet/pull/8331
[hoprnet#8341]: https://github.com/hoprnet/hoprnet/pull/8341
[hoprnet#8345]: https://github.com/hoprnet/hoprnet/pull/8345
[summary]: https://github.com/hoprnet/rfc/blob/0b1eb50cf8e11a312ce3f29e723fc97e1d47bf32/SUMMARY.md

## Design constraints that apply to all of them

**The return path is chosen by the SURB creator, not the replier** (§2.4). A SURB is a
pre-built return header; the exit can only use one as given. So return-path _selection_
lives in the client (`edgli`), and any scenario testing selection must vary the **edgli**
build. The exit's `hoprd` build governs SURB _storage_ — pop order (#8328) and relayer
invalidation (#8329) — so those need the **hoprd** build varied instead. Getting this
backwards produces a test that cannot move.

**Measure the return direction in isolation.** Open the session **0-hop forward, 1-hop
return**. Then no cluster node relays anything outbound, and each node's
`hopr_packets_count{type="forwarded"}` counts exactly the replies it carried — a
return-relayer histogram (`src/relayers.rs`). With a symmetric 1-hop session the forward
relay's count swamps the signal.

**Relayers must have different edge scores or nothing is measurable.** Edge score is
probe-success-rate × a **step function** of latency (§6.3):

| delay   | score |
| ------- | ----- |
| ≤75 ms  | 1.00  |
| ≤125 ms | 0.70  |
| ≤200 ms | 0.30  |
| >200 ms | 0.15  |
| no data | 0.05  |

Path value is the product of edge costs, and paths are sampled **weighted-random** by that
value. On an unshaped local cluster all four relayers score identically, so weighted-random
_is_ uniform and no selection strategy is distinguishable from another — this was measured,
not assumed (see Results). Shape inter-node latency with
`cluster::request_latency_profile`, one node per step; evenly-spaced milliseconds waste
candidates on duplicate scores.

**A 5-node cluster cannot reproduce path pruning.** `hoprd-localcluster` caps at
`MAX_NUM_NODES = 5`, giving the exit 4 return-relayer candidates. Path-finding caps
`max_paths = 8` (§6.3), so with 4 candidate paths the truncation half of the original root
cause never fires. Only the weighted-draw half is testable here; the pruning half needs a
network larger than the local cluster.

**Use the right statistic.** With only 4 candidates, an even rotation gives 25% each and a
weight-proportional draw still only reaches ~36–49% for the best one, so a cap on maximum
share cannot separate them (and flips sign run to run). Use `RelayerSpread::imbalance` —
busiest ÷ least-busy — which tracks the raw score ratio for an untempered draw and its
compressed form once tempering is applied.

**But be honest about its power.** Tempering does not aim at 1.0 — it compresses the ratio
without reordering, so the target is a _smaller_ ratio, not a flat one. Against pre-fix runs
of 2.28 and 4.63 that leaves only a narrow band, and with `wanted = 2` the marginal
distribution is flattened further by the draw itself. A measured value near 2.2 would be
ambiguous rather than conclusive. Separating tempered from untempered selection cleanly
needs more candidates than this cluster has.

## Scenarios

### S1 — Selection spread under skewed scores · **implemented**

_Isolates:_ #8331 — weight tempering (`w^γ`, γ default 0.5) versus a raw weighted draw.

Note the PR was reworked after these measurements: it originally spread a batch over K
distinct first relayers, which the results below showed could not work (K is capped at 2 by
`PAYLOAD_SIZE / HoprSurb::SIZE`). The scenario is unchanged — only what it is measuring is.

_Setup:_ 5 nodes, latency profile one node per score step, 0-hop forward / 1-hop return,
4 MiB pump. Vary the **edgli** pin.

_Signal:_ `imbalance`. A raw weighted draw tracks the edge-score ratio (measured 2.28–4.63
under skew); tempering at γ=0.5 halves the exponent on that ratio.

_Threshold, re-derived for tempering_ (was 1.5, the rotation target, which tempering cannot
reach by design). RFC-0014 scores this profile 1.0 / 0.7 / 0.3 / 0.15, giving raw shares of
46/33/14/7 — a ratio of 6.67. Tempered: 36/30/20/14, a ratio of 2.58. Calibrating the
two-per-packet flattening from the pre-fix run (ideal 6.67 measured 4.63, ×0.69) predicts
**≈1.79**, so the test now allows **2.1**.

⚠️ **Predicted, not measured.** No tempered run has been recorded yet, and 2.1 sits only ~8%
below the lowest pre-fix measurement. Re-derive from a measured tempered baseline before
treating a pass as evidence.

_Test:_ `return_paths_should_spread_across_distinct_relayers`.

### S2 — Return relayer dies (SIGKILL) · **implemented, characterisation only**

_Intended:_ survivability when a relay vanishes without closing anything.

_Actual:_ does not discriminate — see Results. Arrival lands at ~55% whether the killed
relayer carried 25% or 49%, on both stacks. Killing a node removes it from _everything_
at once (transport, probing, all four of its channels), and the session enters a
~0.01 MB/s crawl that the pump's 10 s read-idle timeout ends at the same point every run.
Structural candidates, all independent of selection: SURBs already delivered that name the
dead first hop are unusable and single-use (§2.4); edgli keeps minting more for up to the
60 s path cache / 30 s refresh (§6.3); the exit then signals SURB distress `0x01` /
out-of-SURBs `0x03` (§2.4); and retransmissions can be silently dropped as duplicate
`ReplayTag`s (§2.2), which fits the ~4× packet amplification observed.

_Keep it as:_ a total-collapse guard and behaviour record. Do not cite it as evidence for
any of the four PRs.

_Test:_ `session_should_survive_return_relayer_loss`.

### S3 — Return relayer's channel closes · **not implemented — highest value next**

_Isolates:_ #8329 `invalidate_relayer`, which is keyed on **channel close**, not on node
death. S2 kills the process, so the channel stays `OPEN` on chain and the invalidation
path never fires — S2 cannot test #8329 at all.

_Setup:_ as S1, but instead of SIGKILL, close the exit→R channel from the exit's REST API
(`DELETE /api/v4/channels/{address}`). R stays alive, reachable and well-scored, so this
isolates the invalidation logic from transport death. Vary the **hoprd** pin.

_Signal:_ R's forwarded delta drops to ~0 within one SURB-buffer drain while overall
arrival stays high. Without the fix, SURBs naming R keep being popped and fail: a
non-final edge needs an OPEN channel (§6.1), and `PENDING_TO_CLOSE`/`CLOSED` makes the
ticket unredeemable (§3.1), so those replies are lost rather than rerouted.

_Watch out:_ closure is not instant — `PENDING_TO_CLOSE` holds for `T_closure` (§3.1).
Gate on the channel status the exit reports, not on a sleep.

### S4 — Stale SURB backlog drains before a path change takes effect · **not implemented**

_Isolates:_ #8328 FIFO vs LIFO pop order — the "a return-path change only takes effect
after ~10 MB of stale backlog is consumed" half of the incident. **v4 only.** hoprnet#8473 made
v5 FIFO-only, so on v5 there is no second order to vary and this scenario degenerates into the
baseline it was meant to contrast with. Any future run has to pin a v4 hoprd.

_Setup:_ provision a large SURB buffer and let it fill (the session config already asks for
a production-scale 10 MB / `always_max_out_surbs`), quiesce the reply stream so the backlog
is genuinely stale, close a channel as in S3, then resume.

_Signal:_ time (or bytes) from the change until replies first appear on the new path. FIFO
must drain the backlog first; LIFO uses the freshest SURB immediately. Vary the **hoprd**
pin; the metric is a latency-to-recovery, not a share.

⚠️ **Do not read a result here as evidence about PIX.** Neither order is correct under PIX: both
return a contiguous run of the emission, and a cycle recovers only if *every* polynomial of its
emission reaches threshold, so both bulk-fail a cycle once the ring overflows (measured against
the real reconstructor: 5 % contiguous loss is fatal, 50 % spread loss is not). What bounds that
is the Exit store's emission-window bucketing (hoprnet `SurbShareInfo::emission_window`). This
scenario measures *staleness latency on a reopened path*, which is a different property.

### S5 — 0-hop return path must not be invalidated · **not implemented**

_Isolates:_ #8329's false-positive guard. `chain_length() == 1` means the first relayer
_is_ the final recipient, and the final hop needs no channel (§6.1) — so closing a channel
must **not** invalidate those SURBs.

_Setup:_ 0-hop return path, close a channel from the exit, keep pumping.

_Signal:_ replies continue uninterrupted. A regression here shows up as replies stopping
after an unrelated channel closes — silent and hard to attribute in production, which is
exactly why it is worth pinning.

## Results

Measured on this laptop, 5-node binary chain, 4 MiB pump. "pre-fix" = `hoprd` 4.0.3 +
`edgli` 3.5.0, both on `hopr-lib` `10f6d80c`; "fixed" = `hoprd` 4.1.0 (hoprd#123) +
`edgli` `06ab8d1f` (edge-client#143) on `hopr-lib` `4a47cdff`.

| Config                        | histogram       | imbalance | after-kill arrival |
| ----------------------------- | --------------- | --------- | ------------------ |
| fixed, unshaped               | 25/25/25/25     | 1.03      | 54.86%, 55.06%     |
| pre-fix, unshaped             | 25/25/25/25     | 1.04      | 54.16%             |
| pre-fix, skewed¹              | 36/32/16/16     | 2.28      | —                  |
| pre-fix, skewed¹              | 49/24/17/11     | 4.63      | 54.81%             |
| **fixed, skewed¹**            | **34/28/26/12** | **2.97**  | —                  |
| fixed, skewed¹, debug logging | 33/26/21/20     | 1.63      | **100.00%**        |
| fixed, skewed¹, 0.25 MB/s     | 34/30/24/12     | 2.85      | 36.59%             |

¹ with the first latency profile (5/50/100/150/200 ms), which collides two pairs of nodes
into the same score step; the current profile separates all four. All five rows are
directly comparable — same profile, same cluster shape, same 4 MiB pump.

### What the numbers say

**The after-kill collapse is pre-existing and not attributable to the stack.** Identical on
both stacks, and invariant whether the killed relayer carried 25% or 49%. S2 attributes
nothing to any of the four PRs.

**What the collapse is _not_.** Three hypotheses were formed and each was refuted by a
subsequent run — recorded here so they are not re-proposed:

| Hypothesis                                                       | Refuted by                                                         |
| ---------------------------------------------------------------- | ------------------------------------------------------------------ |
| A gap the PR stack leaves open                                   | pre-fix stack collapses identically (54.16%)                       |
| SURB-balancer counting sent rather than usable SURBs             | plausible, but the instrumented run passed at 100%, so unconfirmed |
| Load dependence — offered rate exceeds post-kill return capacity | 0.24 MB/s gave **both** 100% and 36.6%                             |

**Confirmed: recovery latency.** With a 60s settle between the kill and the replacement
session — every other condition identical to the runs that collapsed — arrival is **100%**
at full speed (4 MiB in 9.02s, 0.47 MB/s), with the dead relayer's share fully redistributed
to the survivors. The controlled series:

| settle            | logging    | offered       | after-kill arrival |
| ----------------- | ---------- | ------------- | ------------------ |
| 0s                | normal     | 0.48 MB/s     | 54.2 – 55.1% (×4)  |
| 0s                | normal     | 0.24 MB/s     | 36.59%             |
| ~24s (incidental) | debug      | 0.24 MB/s     | 100%               |
| **60s**           | **normal** | **0.47 MB/s** | **100%**           |

A session does not collapse from losing a return relayer. It collapses from being
_established inside the recovery window_, when the planner still offers the dead relayer as
a candidate and every SURB minted through it is single-use and burned (§2.4). 60s clears the
60s cache TTL / 30s refresh plus the 5s probe interval; ~11s does not.

**Original hypothesis, now superseded:** The one variable that tracks the outcome is how
long elapses between the kill and the replacement session minting its SURBs — ≈24s when it
survived, ≈11s when it collapsed. Probing runs every 5s and the path cache is 60s TTL / 30s
refresh (§6.2, §6.3), which brackets that window: a session established too soon builds its
return paths from candidates that still include the dead relayer, and each SURB so minted is
single-use and burned (§2.4). Packet counts corroborate — 6567 forwarded for a clean 4 MiB
when it survived, 28–31k for a partial transfer when it did not, i.e. heavy retransmission.

`HOPRD_KILL_SETTLE_SECS` parameterises that delay so it is a controlled variable rather than
an accident of test timing.

**Under skew, the fixed stack does not spread as designed.** Its imbalance (2.97) falls
_between_ the two pre-fix runs (2.28, 4.63) — no separation. The design predicts ≈1.0,
because with 4 candidates `K = min(min_return_path_diversity, buckets) = 4` should select
every bucket and then cycle.

**The mechanism, confirmed.** At the call site (`transport/hopr/src/path/planner.rs`):

```rust
let num_possible_surbs = HoprPacket::max_surbs_with_message(size_hint).min(max_surbs);
self.resolve_diverse_return_paths(*destination, return_options, num_possible_surbs)
// …and inside: let wanted = self.min_return_path_diversity.min(count);
```

`count` is how many SURBs fit in **one packet**. Measured from `hopr-crypto-packet`:

```text
PAYLOAD_SIZE = 1038 B   HoprSurb::SIZE = 395 B
MAX_SURBS_IN_PACKET = 2         max_surbs_with_message(1400) = 0
```

So `wanted = min(min_return_path_diversity, 2) = 2`, always. Every SURB-carrying packet
selects **2 of the available relayers** via `pick_distinct_buckets` — weighted, and freshly
re-drawn on every call, because `resolve_diverse_return_paths` keeps no state between
calls. The round-robin only rotates within that pair, so the marginal distribution over
relayers stays weight-proportional. Measured imbalance 2.97 with four candidates, against
a design target of 1.0.

Two consequences:

- **`min_return_path_diversity` cannot exceed 2 in practice.** Its default of 6 is
  unreachable — the ceiling is packet geometry (`PAYLOAD_SIZE / HoprSurb::SIZE`), not
  configuration. A validator on the config cannot express this; only the call site can.
- **The "a dead relay owns ~1/K" property degrades to K = 2**, i.e. up to ~50% of the
  return stream, and more when the dead relayer was the better-scored member of the pair.
  The argument for closing hoprnet#8332 assumed K ≥ 6 keeping loss near 17%, below the
  ~20% a reliable session tolerates. That premise does not hold at any configured value.

**Cache expiry is the wrong recovery trigger.** The path cache is 60 s TTL / 30 s
background refresh (§6.3) with no invalidation on timeout or loss. After a relayer dies the
client keeps minting SURBs naming it — weight-proportionally, so a _well-scored_ dead
relayer attracts _more_ of them — and each is single-use (§2.4), so they are burned rather
than retried elsewhere. The exit then signals SURB distress `0x01` / out-of-SURBs `0x03`.
Recovery should be driven by observed timeouts/loss, not by a timer.
