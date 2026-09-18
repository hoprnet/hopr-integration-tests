# PIX under end-user traffic shapes

What a PIX Session does under the traffic a VPN client actually produces, measured rather than
derived. The scenarios are `integration/tests/pix_shapes.rs`; the profile they run at is
`integration/src/shapes.rs`.

Run them with:

```bash
HOPRD_SRC=../hoprd-shapes just pix-shapes            # all of them, hours
HOPRD_SRC=../hoprd-shapes just pix-shapes <scenario> # one, ~12-20 min
```

`HOPRD_SRC` must be a hoprd carrying the `--pix-config` seam. `just pix-shapes` refuses to run
without it, because a `hoprd-localcluster` that ignores `--pix-config` silently runs the demo
geometry and every assertion below would then be measuring a cycle 2 500x smaller than it claims.
Exit-side PIX fill (hoprnet#8396) is no longer a condition worth stating: it has been on hoprd
`main` since hoprd#166.

All three sides of a run must still be on one hoprnet rev — **`11b5f1b6`** as of this writing.
A skew is not subtle but it is silent from here: two sources put two `hopr-lib`s in the lock, the
entry runs one and registers its metrics in the other, and every counter reads zero.

**Agreeing on the commit is not enough — the git *reference* has to match too.** edge-client#170
moved its own pin from a `rev` to `branch = "master"`, and naming a `rev` here that resolves to the
identical commit still splits the graph. Measured on this manifest:

```
git+https://github.com/hoprnet/hoprnet?branch=master#11b5f1b6...
git+https://github.com/hoprnet/hoprnet?rev=11b5f1b6...#11b5f1b6...
```

Two entries, same commit. So `integration/Cargo.toml` tracks `master` because edgli does, and the
exact commit lives in `Cargo.lock` — set with `cargo update -p hopr-lib --precise <sha>`, which is
reproducible for anyone building from the committed lock. The cost is that a bare `cargo update`
walks to hoprnet's tip, which is not necessarily what the Exit binary was built from; check it
against `hoprd`'s `Cargo.toml` when you touch the lock.

**A dependency's own lock is ignored by its consumers, which is how a green CI hides a break.**
hoprnet#8430 dropped `pix_ssa_quota` from `HoprSessionClientConfig`; edgli `main` still filled it,
and did not notice, because edgli's lock held `hopr-lib` several commits behind the very branch its
manifest names. From here the branch resolves to its tip and the build simply fails. Fixed in
edge-client#175. Expect this shape of failure again: edgli's CI being green says nothing about
whether it builds at `master`'s tip.

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

Measured on a 3-node local cluster, 1 hop, binary chain.

Counters are the Exit's `hopr_strategy_pix_*` delta over the scenario; **paid** is the rise in the
Exit's *Safe balance*, read from the chain. Those two columns were read at hoprnet **`57b5e679`**,
where all six passed. The **wall** column carries both that run and the re-run at **`11b5f1b6`**,
where five of the six still pass — the same assertions, payment floor included.

| shape | traffic offered | recovered | swept | deposits | paid (wxHOPR) | wall `57b5e679` | wall `11b5f1b6` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **geometry spike** | 88 MB at 300 pkt/s, loopback | 1 | 1 | 2 | 0.08503296 = **1×** | 593 s | 595 s |
| **idle** | 25 keep-alives of 32 B, one per 25 s | 1 | 1 | 2 | 0.08503296 = **1×** | 817 s | 795 s |
| **browsing** | 40 kB bursts every 3 s (~13 kB/s), 540 s | 1 | 1 | 2 | 0.08503296 = **1×** | 745 s | 712 s |
| **download** | service pushes 2 cycles at 300 pkt/s | 2 | 2 | 3 | 0.17006592 = **2×** | 753 s | 719 s |
| **upload** | 300 pkt/s into a sink, nothing returns | 1 | 1 | 2 | 0.08503296 = **1×** | 690 s | 690 s |
| **mixed** | browse 180 s → quiet 120 s → bulk 1 cycle | 2 | 2 | 3 | 0.17006592 = **2×** | 1113 s | **failed** |

No passing scenario recorded a deposit timeout, and none was closed by the supervisor.

### The mixed shape fails at `11b5f1b6`, and not on anything PIX does

It failed its *first* assertion — that traffic resumes after the quiet phase — with **0 %** of the
bulk payload arriving, before any counter or balance was read. The transport, not the strategy:

```
08:04:44  quiet begins (120 s, no application traffic at all)
08:04:54  every p2p connection drops:  num_peers 2 → 1 → 0
08:04:58  "cannot find 1 hop path ... in the channel graph" begins
08:06:26  peers reconnect (2), 08:06:59 (3)
08:07:46  bulk gives up: 0/88473600 B, ttfb never, 60 s idle budget spent
08:08:06  path resolution recovers — 20 s too late
```

No channel was closed on chain and the supervisor never intervened; the peers came back on their
own. What did not come back in time was the *channel graph*, which stayed unresolvable for ~100 s
after the connections were restored. The 60 s idle budget expires inside that window.

**This is one observation, and it is not yet diagnosed.** The re-run that would say whether it is a
regression or load flakiness has not happened — an unrelated cluster was holding port 8545. The
honest reading for now: the five other shapes are unaffected, this one is quarantined, and the
suspects are the transport changes in this rev range (hoprnet#8424, #8426) rather than anything in
the PIX path. Do not quote the old 1113 s as a current result.

**Every scenario's Safe delta was an exact whole multiple of the per-SSA deposit** — `1×` or `2×`
of 0.08503296 wxHOPR, matching the cycles its counters claimed, with no residue from relayed-ticket
income. The assertion only enforces a floor (see `assert_exit_was_paid` for why), but exactness
held in all six at `57b5e679`. At `11b5f1b6` only the floor is known to have held, for the five
that ran: the harness prints the scenario's own counters on failure, not on success.

This matters because the counters alone cannot establish it. `keys_recovered` and `sweeps` are the
Exit's own bookkeeping, and there is a path where both increment and no money moves: a cycle that
completes before its deposit is mined leaves the Exit recovering a key against a zero balance,
logging "already swept", with the funds stranded at the stealth address. Until the balance check
was added, all six scenarios would have reported success against an Exit that was never paid.

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

## Two findings that are not about PIX, and one correction

**The entry closed its own channels ten minutes in, twice, for two different reasons.**
`close_below_quality_score` defaults to 0.3 and is separate from the eligibility threshold this
harness already zeroes. Probing needs traffic to score a peer; an idle shape offers 32 bytes every
25 s; the score decays and the strategy closes the channel underneath a Session that is working
perfectly. Measured: channels opened at T+0, closed at T+10m07s, then 17 522 consecutive
`cannot find 1 hop path` failures.

Zeroing that threshold was not enough — a later run closed two of three channels at T+10m07s again.
The close pass stops only when `remaining_open <= min_open_channels`, and the harness set that floor
to 1 while opening 3. `env.rs` now sets the floor equal to the target, which forbids the close pass
outright regardless of which trigger fires; the quality threshold stays as defence in depth.

For a deployment this is worth knowing rather than alarming — a real network has more relays and
more probing traffic — but an idle VPN client is exactly the case that produces neither, and the
interaction between a quiet Session and quality-scored channel closure is real.

**The echo pump cannot express an asymmetric shape.** `pump_halves`' reader decides completion
against the payload that was *sent*, because every other scenario here targets the Exit's loopback.
A download's reply volume is the service's choice, so the pump declared `Complete` after 0.23 s
against a 491 s push; an upload's is zero, so it would have declared `NeverStarted` at 30 s. Both
shapes now drive the Session directly.

**The asymmetric scenarios used to run 20 minutes past their own measurement.** `drain_for` and
`offer_for` ran to their full budget rather than stopping when the sweeps landed, so the download
kept reading for 20 minutes after the service had pushed its last byte. It passed either way — the
cost was wall-clock, and it is worth stating as wall-clock: the download took **1922 s** before the
stop flag and **715 s** after, for the same result.

An earlier revision of this file claimed the same change fixed a CPU spin, on the strength of a
process sampled at ~7.5 cores. That was a misdiagnosis and is withdrawn: the process sampled was a
later scenario's binary, not the download's, which had already exited; and a test binary hosting
the edgli Entry in-process alongside five `hoprd` nodes, anvil and blokli on one machine is
expected to be busy. Nothing here measured what an idle Session costs.

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
