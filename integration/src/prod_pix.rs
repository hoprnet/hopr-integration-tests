//! The **deployed** PIX geometry, and the arithmetic that makes running it a scenario rather than
//! a transcription.
//!
//! [`crate::shapes`] runs PIX at a geometry deliberately unlike production and argues at length
//! that the *ratios* are what govern PIX, so a five-minute cycle at a local rate reproduces the
//! deployed behaviour at any rate a cluster can carry. That argument is sound, and it is also the
//! one thing this module exists to stop being taken on faith.
//!
//! What no scaled profile can exercise is the part of PIX that is *not* a ratio:
//!
//! * **The numbers are the deployed ones.** `4096 × (32 + 79)` is what hoprnet#8519 ships as
//!   `DEFAULT_POLYS_PER_SSA`, `DEFAULT_POLY_THRESHOLD` and `DEFAULT_SURPLUS_SHARES`, and it is what
//!   the Exit's default `quota_range` is derived from. A scaled split can agree with production on
//!   every ratio and still sit outside the window the Exit will actually admit, which is a failure
//!   that only shows up against the real dimensions.
//! * **The loss tolerance is a property of the split, not of the ratio.** `s/(t+s)` is break-even;
//!   the loss a cycle survives is `P(Binom(t+s, 1-L) >= t)^polys`, and the exponent is what turns a
//!   gentle per-polynomial slope into a cliff (see the PIX loss-tolerance reference). Running the
//!   real `polys` is the only way to exercise the real exponent.
//! * **The cycle is priced and timed as production times it.** A cycle is `polys × (t + s)` =
//!   454 656 packets and one deposit is `price_per_byte × quota` over a 629.6 MiB quota. That is
//!   the quantity the Exit's `max_recovery_time` floor, its live-cycle memory reservation and the
//!   Entry's successor gate are all sized against.
//!
//! # Why this fits in minutes
//!
//! 454 656 packets is a lot, and it is the reason the deployed cycle is quoted at 306 s nominal —
//! but a cycle is bounded by *packets*, not by seconds, and a local cluster carries far more than
//! the deployed network. hoprd's own soak and the PIX demo both measure ~4 000–8 500 datagrams/s
//! each way on this class of machine, so a cycle lands at ~54–114 s rather than five minutes, and
//! the three cycles a scenario wants are ~6 min of traffic on top of the ~3 min of cluster
//! bootstrap — a run that lands around ten minutes rather than under five.
//!
//! # What is deliberately unlike production
//!
//! Nothing about the geometry. Two things about the *sizing around* it, both forced by a test
//! having finite wall-clock and a local chain:
//!
//! * [`MAX_RECOVERY_TIME`] is the upstream default (four hours) *in the cluster config*, but a
//!   scenario never waits for it — it waits on the cycles it can count. Stating it rather than
//!   defaulting keeps the validator's floor check against `quota_range_max` honest.
//! * [`MAX_DEPOSIT_WAIT`] is 60 s rather than upstream's own, matching the localcluster demo: an
//!   Anvil block is ~1 s and a public chain's ~6 s, so the deployed value leaves a cycle far more
//!   room than a local one needs, and the kill-switch fuse is the *sum* of the two deadlines.
//!
//! # Overrides
//!
//! The packet rate is read from `HOPRD_PROD_PIX_RATE` at run time so a run can be tuned without a
//! recompile; see [`packet_rate`].

use std::time::Duration;

use edgli::hopr_lib::exports::transport::SESSION_MTU;

// ── Geometry: the deployed split, restated ───────────────────────────────────

/// Polynomials per SSA — hoprnet's `DEFAULT_POLYS_PER_SSA`.
pub const PIX_POLYS: usize = 4096;

/// Shares needed to reconstruct one polynomial — hoprnet's `DEFAULT_POLY_THRESHOLD`.
pub const PIX_SHARES: usize = 32;

/// Shares emitted beyond the threshold — hoprnet's `DEFAULT_SURPLUS_SHARES`, which is
/// `default_surplus_for(4096, 32)`.
///
/// Not a free parameter: it is the fewest surplus shares with which a cycle of 4096 polynomials
/// still recovers at 99 % when every share is lost with probability one half. The value is pinned
/// by [`the_deployed_split_tolerates_half_the_shares`] against the closed form, so a change to the
/// upstream derivation that moves it fails here rather than silently moving what this exercises.
pub const PIX_ADDITIONAL_SHARES: usize = 79;

/// `E` — every share one cycle emits, and therefore every packet a cycle takes to complete.
pub const CYCLE_PACKETS: u64 = (PIX_POLYS * (PIX_SHARES + PIX_ADDITIONAL_SHARES)) as u64;

/// Bytes one share is priced at, as the Exit computes it: one Session segment.
pub const QUOTA_BYTES_PER_SHARE: u64 = SESSION_MTU as u64;

/// Bytes one cycle costs the Entry — the per-SSA quota the Exit prices its deposit against.
///
/// 4096 × 111 × 1452 = 660 160 512 B = 629.6 MiB, which is exactly hoprnet#8519's headline figure
/// and exactly the Exit's `DEFAULT_PIX_SSA_QUOTA`.
pub const QUOTA_PER_SSA: u64 = CYCLE_PACKETS * QUOTA_BYTES_PER_SHARE;

// ── Rate and the cycle it implies ────────────────────────────────────────────

/// Return packets per second the scenario drives, overridable via `HOPRD_PROD_PIX_RATE`.
///
/// 4 000, the rate hoprd's own PIX demo commits to and the top of the 3 000–4 000 band that demo
/// measured on this class of machine. The soak measures the flow sustaining 6 000 datagrams/s each
/// way and saturating by 6 500, so this is a third clear of the knee — the subject is whether the
/// *deployed geometry* completes and recovers, not what the runner can do, and a rate the machine
/// cannot hold stretches cycles into the Exit's deposit deadline.
///
/// The figure is a *return* packet rate — one share rides on each Exit → Entry reply — so it is
/// also the share rate, and a cycle takes `CYCLE_PACKETS / rate` seconds: 114 s nominal here,
/// measured ~116 s (the Exit's egress is shaped by its SURB buffer, but at this rate the achieved
/// cycle lands close to nominal). The deployed cycle is 306 s, so this is roughly a third of
/// production's.
pub const DEFAULT_PACKET_RATE: u64 = 4_000;

/// Packets per second the scenario drives, honouring `HOPRD_PROD_PIX_RATE`.
pub fn packet_rate() -> u64 {
    std::env::var("HOPRD_PROD_PIX_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r| *r > 0)
        .unwrap_or(DEFAULT_PACKET_RATE)
}

/// Nominal seconds per cycle, `E / rate`.
///
/// Nominal only. The achieved figure is longer because the Exit's egress is shaped by its SURB
/// buffer and because shares share the return path with echoed data — hoprd's soak measures 2–3×
/// at low rates and ~1× at 4 000. Deadlines are sized against the achieved figure, thresholds
/// against the nominal one, and the two are never mixed.
pub fn nominal_cycle_secs() -> u64 {
    CYCLE_PACKETS / packet_rate().max(1)
}

// ── Exit-side deadlines ──────────────────────────────────────────────────────

/// Exit's deadline for the SSA commitment to arrive.
pub const MAX_SSA_DELIVERY_TIME: Duration = Duration::from_secs(20);

/// Exit's deadline for the deposit to land.
///
/// The localcluster demo's 60 s rather than upstream's own: an Anvil block is ~1 s where a public
/// one is ~6 s, and the kill-switch fuse is the sum of this and [`MAX_SSA_DELIVERY_TIME`]. A
/// *cycle* must still outlast a deposit round trip by a wide margin — the compile-time guard below
/// states it — or the Exit reconstructs a key before its deposit is mined, logs "already swept",
/// and strands the funds at the stealth address.
pub const MAX_DEPOSIT_WAIT: Duration = Duration::from_secs(60);

/// Absolute per-cycle recovery deadline, and the idle tariff.
///
/// Upstream's four hours, which is what a deployed Exit runs. Nothing here waits it out — a
/// scenario counts cycles, and every one of them completes in a minute or two — but it is stated
/// because `validate_incoming_session_pix_config` computes a floor against `quota_range_max` and a
/// scenario that lowered it would be measuring a configuration no Exit ships.
pub const MAX_RECOVERY_TIME: Duration = Duration::from_secs(4 * 3600);

/// Fraction of [`MAX_RECOVERY_TIME`] at which Exit fill aims to have an idle cycle finished.
///
/// Upstream's default. Irrelevant to a scenario that drives traffic every cycle, and stated only
/// because the cluster config carries it.
pub const FILL_FINISH_FRACTION: f64 = 0.75;

/// Ceiling on the Exit's self-generated fill traffic, packets/s. Upstream's default.
pub const FILL_MAX_RATE: u32 = 250;

// ── Admission window ─────────────────────────────────────────────────────────

/// Lower bound of the quota window the Exit accepts.
///
/// The Exit's own default floor is `DEFAULT_PIX_SSA_QUOTA / 4`, i.e. one quarter of a deployed
/// cycle. Stated as that expression rather than as a literal so the two move together.
pub const QUOTA_RANGE_MIN: u64 = QUOTA_PER_SSA / 4;

/// Upper bound of the quota window the Exit accepts.
///
/// Exactly one deployed cycle, which is the Exit's own default maximum: `DEFAULT_PIX_SSA_QUOTA`
/// *is* `polys × (t + s) × MTU` at the deployed split. Anything above it would let this profile
/// drift from production without the run noticing.
pub const QUOTA_RANGE_MAX: u64 = QUOTA_PER_SSA;

// ── Session sizing (Entry side) ──────────────────────────────────────────────

/// SURBs the Entry keeps buffered at the Exit.
///
/// `8 × (share_rate + data_rate)`: eight seconds of runway over both return streams, since one
/// share rides on each reply and the echo shares the same budget. This is hoprd's own
/// `SURB_RUNWAY_SECS` sizing, and it is a *floor* rather than a throughput dial — a deeper buffer
/// is a longer pipeline between a share being generated and delivered, not a faster one, and a
/// buffer deeper than a cycle parks share-less SURBs at the head of the Exit's FIFO that no rate
/// of fill can get past.
///
/// The ceiling is the balancer target ceiling, `2/3 × rb_capacity` = 66 666 SURBs; the guard below
/// checks the default rate clears it with room, and [`surb_buffer_target`] is where an override is
/// judged.
pub const SURB_RUNWAY_SECS: u64 = 8;

/// The largest SURB buffer *target* a Session may ask for — the balancer's target ceiling.
///
/// `2/3 × rb_capacity` = 66 666 SURBs, and the value the Exit clamps an announced buffer target to.
/// It is not the ring's own size: the ring holds the full `rb_capacity` = 100 000 and only evicts
/// there, well above this. What the ceiling bounds is the *target* the Entry's balancer aims for, so
/// [`surb_buffer_target`] keeps the target strictly below it and the two sides agree on the depth.
pub(crate) const SURB_BUFFER_CEILING: u64 = 66_666;

/// SURBs the Entry buffers at the Exit, honouring `HOPRD_PROD_PIX_SURB_RUNWAY`.
///
/// The share rate is `CYCLE_PACKETS / nominal_cycle_secs`, which is `packet_rate()` by
/// construction — a cycle takes `E / rate` seconds and emits `E` shares — so the two terms collapse
/// and this is `runway × 2 × rate`. Written as the sum anyway, because the identity is a
/// consequence of the pacing rather than something to rely on silently.
///
/// An override that would reach [`SURB_BUFFER_CEILING`] is clamped to one below it rather than
/// honoured: the Exit clamps an announced target to the same 66 666, so a larger target would leave
/// the Entry's *own* balancer aiming at a depth the Exit never honours, the two sides disagreeing
/// about the buffer. The clamp aligns the Entry's target with what the Exit will actually use; it is
/// not a defence against ring eviction, which this knob cannot reach. It warns rather than failing,
/// so an experiment stays runnable while saying what it lost.
pub fn surb_buffer_target() -> u64 {
    let runway = std::env::var("HOPRD_PROD_PIX_SURB_RUNWAY")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r| *r > 0)
        .unwrap_or(SURB_RUNWAY_SECS);
    let target = runway * 2 * packet_rate();
    if target >= SURB_BUFFER_CEILING {
        tracing::warn!(
            target,
            ceiling = SURB_BUFFER_CEILING,
            clamped = SURB_BUFFER_CEILING - 1,
            "the SURB buffer target reaches the ceiling, so it was clamped below it; a larger \
             target would have the Entry aiming at a depth the Exit never honours, leaving the two \
             sides disagreeing about the buffer"
        );
        return SURB_BUFFER_CEILING - 1;
    }
    target
}

/// Ceiling on SURB production, per second.
///
/// `4 × rate` with headroom: one SURB is consumed per return packet, so anything below `rate`
/// starves the Exit no matter how deep the buffer is.
pub fn max_surbs_per_sec() -> u64 {
    4 * packet_rate()
}

/// Packets the Exit serves without a share coming back before its egress gate blocks, honouring
/// `HOPRD_PROD_PIX_MAX_SERVED`.
///
/// The buffer plus a second of replies, which is what covers the drain after a recovery: the SURBs
/// still queued carry the recovered cycle's shares, and the gate must not block part-way through
/// spending the very SURBs that are draining it.
pub fn max_served_without_progress() -> u64 {
    std::env::var("HOPRD_PROD_PIX_MAX_SERVED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| surb_buffer_target() + packet_rate())
}

/// Whether the Exit fills a cycle the application has left unfinished, honouring
/// `HOPRD_PROD_PIX_FILL` (`0` / `false` to disable).
///
/// On by default, as upstream ships it. A scenario that drives traffic every cycle never needs it;
/// it is on so the configuration is the deployed one rather than a test-only variant.
pub fn fill_enabled() -> bool {
    !matches!(
        std::env::var("HOPRD_PROD_PIX_FILL").as_deref(),
        Ok("0") | Ok("false")
    )
}

// ── Compile-time guards ──────────────────────────────────────────────────────

/// The deposit round trip a cycle has to outlast: hoprd's own measurement of the Exit observing a
/// deposit (2.2 s) and the Entry confirming it (5.0 s).
///
/// This, not [`MAX_DEPOSIT_WAIT`] + [`MAX_SSA_DELIVERY_TIME`], is the quantity a cycle must beat.
/// The two deadlines are the Exit's *kill switch* — how long it will serve unincentivized before
/// closing the Session — and a cycle comfortably inside them is a different (and much weaker)
/// statement than a cycle comfortably longer than the round trip. Getting the two confused is what
/// the sentence in `session_pix_soak` warns about: the Exit reconstructs the key before the money
/// lands, `sweep_recovered` finds a zero balance, logs "already swept", and the deposit is
/// stranded at the stealth address for good.
const DEPOSIT_ROUND_TRIP_SECS: u64 = 5;

/// A cycle must outlast a deposit round trip by a wide margin.
///
/// Judged at the *default* rate rather than at a `HOPRD_PROD_PIX_RATE` override, because an
/// override is an experiment and the guard exists to catch the default drifting into the race. At
/// 4 000 packets/s a cycle is 114 s against a 5 s round trip — 23x — and the achieved cycle is
/// longer still because the Exit's egress is shaped by its SURB buffer.
const _: () = assert!(
    CYCLE_PACKETS / DEFAULT_PACKET_RATE >= 3 * DEPOSIT_ROUND_TRIP_SECS,
    "at the default rate a cycle is too close to a deposit round trip"
);

/// ...and must still fit inside the Exit's kill switch, or the Exit closes the Session before the
/// cycle it is paying for has finished.
const _: () = assert!(
    CYCLE_PACKETS / DEFAULT_PACKET_RATE <= MAX_DEPOSIT_WAIT.as_secs() * 4,
    "at the default rate a cycle outlasts the Exit's deposit deadline several times over"
);

/// The offered quota has to land inside the window the Exit accepts, or every Session is refused
/// with `UnacceptablePixParams` before a byte moves.
const _: () = assert!(
    QUOTA_RANGE_MIN <= QUOTA_PER_SSA && QUOTA_PER_SSA <= QUOTA_RANGE_MAX,
    "the offered quota falls outside the accepted window"
);

/// The deployed quota must be exactly the Exit's own default maximum, or this profile is not
/// running the deployed geometry however much its dimensions agree.
///
/// `1452` is the Session MTU on the v5 line, which is the only line PIX exists on; the v4 line
/// prices a share at the whole HOPR packet payload instead, so the literal is checked only there.
/// The relation itself — quota = cycle packets x bytes per share — is checked on both lines.
const _: () = assert!(
    QUOTA_PER_SSA == CYCLE_PACKETS * QUOTA_BYTES_PER_SHARE,
    "the quota is not the cycle's packet count times the bytes a share is priced at"
);
#[cfg(feature = "v5")]
const _: () = assert!(
    QUOTA_PER_SSA == 4096 * (32 + 79) * 1452,
    "the quota is not the deployed polys x (threshold + surplus) x SESSION_MTU"
);

// ── Rendering ────────────────────────────────────────────────────────────────

/// wxHOPR per byte of quota, on both sides: what the Exit requires and the Entry pays.
///
/// hoprd's own soak price (`session_pix_soak.rs`'s `PRICE_PER_BYTE`), which is 13x the deployed
/// 5.33e-8 rather than a fraction of it. The direction is deliberate and it is the *quota* that
/// carries the scaling: this profile's cycle is 629.6 MiB against the soak's ~30 MB, so a price
/// held at the soak's level leaves a deposit of the same order as the soak's. Nothing in the
/// protocol reads the absolute figure — it is the *product* with the quota that both sides check —
/// so the same product measures the same exchange whatever the factorisation. At this price one
/// cycle is 0.000000715 × 660 160 512 ≈ 472 wxHOPR, which the entry's Safe is funded to cover many
/// times over (see [`node_token_target_wxhopr`]) and which is well under
/// [`max_ssa_allocation_wxhopr`], this profile's own ~944 wxHOPR ceiling of twice one cycle — the
/// `MAX_SSA_ALLOCATION` the demo profile states is a 10 wxHOPR ceiling and would not admit a
/// deployed cycle at all.
pub const PRICE_PER_BYTE_WXHOPR: f64 = 0.000_000_715;

/// Ceiling on a single deposit. Twice one cycle, so a cycle can never be refused for being
/// marginally over.
pub fn max_ssa_allocation_wxhopr() -> f64 {
    per_cycle_wxhopr() * 2.0
}

/// Cycles the Entry's budget covers.
///
/// Well above what any scenario here completes, so the budget never binds: a run that ends because
/// the Entry could not pay has measured its own budget rather than the geometry. Ten cycles is
/// ~4 700 wxHOPR, which the localcluster's 1 000 wxHOPR Safe float does *not* cover — so the
/// budget is stated and the *float* is what actually bounds a run. See
/// [`safe_deposit_float_wxhopr`].
pub const BUDGETED_CYCLES: u64 = 10;

/// wxHOPR one completed cycle costs the Entry.
pub fn per_cycle_wxhopr() -> f64 {
    QUOTA_PER_SSA as f64 * PRICE_PER_BYTE_WXHOPR
}

/// wxHOPR the Entry's Safe must hold for [`BUDGETED_CYCLES`] cycles.
///
/// Written into the cluster config as `safe_deposit_float`, which is what localcluster grants each
/// *cluster node's* Safe. The localcluster default is 1 000 wxHOPR, which at this geometry covers
/// barely two cycles — so a scenario that did not raise it would end on an exhausted float rather
/// than on the cycle target, and every assertion after that would be reporting the wrong cause.
pub fn safe_deposit_float_wxhopr() -> f64 {
    per_cycle_wxhopr() * BUDGETED_CYCLES as f64
}

/// `HOPRD_NODE_TOKEN_TARGET` — the wxHOPR localcluster funds each identity's Safe with.
///
/// **This, and not [`safe_deposit_float_wxhopr`], is what funds the paying Entry**, which is the
/// opposite of what the name suggests and cost this scenario two cycles before it was found.
/// `PixSettings::safe_deposit_float` is applied only to the cluster nodes
/// (`pix_safe_target_for` in hoprd's localcluster); an *extra* identity — which is what the
/// in-process edgli boots on — has its Safe deployed with the bare `HOPRD_NODE_TOKEN_TARGET`, and
/// nothing adds the float on top. At the localcluster default of 1 000 wxHOPR that is two cycles at
/// this geometry, after which the Entry silently stops depositing, the Exit's deposit observer
/// times out 30 s later, and the Session closes on `PixFailure` — which reads as a broken surplus
/// rather than as an unfunded account.
///
/// Measured: a run with the default target deposited twice and then closed with
/// `gave up waiting for the deposit ... target=472.01476608 wxHOPR max_tracking=30s`.
///
/// The base is localcluster's own default, so the identity is funded the way every other scenario
/// funds it, plus the cycles this profile needs.
pub fn node_token_target_wxhopr() -> f64 {
    1_000.0 + safe_deposit_float_wxhopr()
}

/// `HOPRD_NODE_TOKEN_TARGET` as the string localcluster parses.
pub fn node_token_target() -> String {
    format!("{:.4} wxHOPR", node_token_target_wxhopr())
}

/// This profile as the YAML `cluster::request_pix_settings` passes to `--pix-config`.
///
/// Every field the profile has a reason to move is named; the rest stay at the localcluster demo
/// default. `price_per_byte` and the spend ceilings are stated because the deposit scales with the
/// quota: at the demo price a deployed cycle would cost ~472 wxHOPR against a 10 wxHOPR per-deposit
/// ceiling, and every deposit would be refused.
pub fn cluster_pix_yaml() -> String {
    let per_cycle = per_cycle_wxhopr();
    format!(
        "\
num_ssa_parts: {polys}
ssa_part_size: {shares}
additional_shares: {surplus}
quota_range_min: {quota_min}
quota_range_max: {quota_max}
max_ssa_delivery_time: {delivery}s
max_deposit_wait: {deposit}s
max_recovery_time: {recovery}s
max_served_without_progress: {served}
fill_enabled: {fill}
fill_max_rate: {fill_rate}
fill_finish_fraction: {fill_finish}
price_per_byte: \"{price:.10} wxHOPR\"
max_ssa_allocation: \"{allocation:.4} wxHOPR\"
max_spend_per_window: \"{budget:.4} wxHOPR\"
safe_deposit_float: \"{float:.4} wxHOPR\"
",
        polys = PIX_POLYS,
        shares = PIX_SHARES,
        surplus = PIX_ADDITIONAL_SHARES,
        quota_min = QUOTA_RANGE_MIN,
        quota_max = QUOTA_RANGE_MAX,
        delivery = MAX_SSA_DELIVERY_TIME.as_secs(),
        deposit = MAX_DEPOSIT_WAIT.as_secs(),
        recovery = MAX_RECOVERY_TIME.as_secs(),
        served = max_served_without_progress(),
        fill = fill_enabled(),
        fill_rate = FILL_MAX_RATE,
        fill_finish = FILL_FINISH_FRACTION,
        price = PRICE_PER_BYTE_WXHOPR,
        allocation = max_ssa_allocation_wxhopr(),
        budget = per_cycle * BUDGETED_CYCLES as f64,
        float = safe_deposit_float_wxhopr(),
    )
}

// ── Entry side ───────────────────────────────────────────────────────────────

/// This profile as the Entry's own generator dimensions.
///
/// Must agree with what [`cluster_pix_yaml`] gives the Exit: edgli derives the quota it announces
/// from these and nothing else, and the Exit refuses any Session whose quota falls outside the
/// window that YAML sets. [`install_profile`] is what keeps the two from being set separately.
#[cfg(feature = "v5")]
pub fn entry_dimensions() -> edgli::PixGlobalConfig {
    edgli::PixGlobalConfig {
        num_ssa_parts: PIX_POLYS,
        ssa_part_size: PIX_SHARES,
        additional_shares: Some(PIX_ADDITIONAL_SHARES),
        ..Default::default()
    }
}

/// wxHOPR one completed cycle costs the Entry, as a parsed balance.
#[cfg(feature = "v5")]
pub fn per_cycle() -> anyhow::Result<crate::HoprBalance> {
    let price: crate::HoprBalance = format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR").parse()?;
    Ok(price * QUOTA_PER_SSA)
}

/// The Entry's settlement configuration for this profile, budgeted at [`BUDGETED_CYCLES`].
#[cfg(feature = "v5")]
pub fn entry_config() -> anyhow::Result<edgli::PixEntryConfig> {
    let per_cycle = per_cycle()?;
    Ok(edgli::PixEntryConfig {
        strategy: edgli::PixEntryStrategy {
            price_per_byte: format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR").parse()?,
            // Twice one deposit: a ceiling, not a budget, and one cycle must never be marginally
            // over it.
            max_ssa_allocation: per_cycle * 2u64,
            // Well above what any scenario completes, so a run that ends on the budget has gone
            // wrong rather than finished — see [`BUDGETED_CYCLES`].
            max_spend_per_window: per_cycle * BUDGETED_CYCLES,
            spend_window: crate::pix::SPEND_WINDOW,
            ..Default::default()
        },
        pool: edgli::PixEntryPool {
            // Must stay under the Exit's `max_deposit_wait + max_ssa_delivery_time` (80 s here), or
            // only the single immediate balance check happens before the Exit gives up.
            max_deposit_tracking_time: Duration::from_secs(40),
            ..Default::default()
        },
        // Only the Curvy pool keeps durable state; `pix-test` selects secp256k1, which ignores it.
        state_dir: None,
    })
}

/// The Entry's SURB balancer for this profile.
///
/// `target_surb_buffer_size` is the depth [`surb_buffer_target`] names, and `max_surbs_per_sec` the
/// ceiling [`max_surbs_per_sec`] names — the two knobs a PIX Session has a measured failure on both
/// sides of.
#[cfg(feature = "v5")]
pub fn surb_balancer() -> edgli::hopr_lib::exports::transport::SurbBalancerConfig {
    edgli::hopr_lib::exports::transport::SurbBalancerConfig {
        target_surb_buffer_size: surb_buffer_target(),
        max_surbs_per_sec: max_surbs_per_sec(),
        ..Default::default()
    }
}

/// Point both halves of the run at the deployed geometry, before the cluster is brought up.
///
/// The Exit's admission window and the Entry's announced geometry are read by different processes
/// and neither can derive the other, so this is the one call that sets them together.
#[cfg(feature = "v5")]
pub fn install_profile() {
    crate::cluster::request_pix_settings(cluster_pix_yaml());
    crate::pix::request_dimensions(entry_dimensions());
    // The paying Entry is the *extra* identity, whose Safe localcluster funds from
    // `HOPRD_NODE_TOKEN_TARGET` alone — `safe_deposit_float` never reaches it. See
    // [`node_token_target_wxhopr`]: without this the run stops after two cycles on an account it
    // never noticed was empty.
    crate::cluster::request_node_env([("HOPRD_NODE_TOKEN_TARGET", node_token_target())]);
    tracing::info!(
        polys = PIX_POLYS,
        shares = PIX_SHARES,
        surplus = PIX_ADDITIONAL_SHARES,
        cycle_packets = CYCLE_PACKETS,
        quota = QUOTA_PER_SSA,
        packet_rate = packet_rate(),
        nominal_cycle_secs = nominal_cycle_secs(),
        surb_buffer = surb_buffer_target(),
        max_served_without_progress = max_served_without_progress(),
        per_cycle_wxhopr = per_cycle_wxhopr(),
        "installed the DEPLOYED PIX geometry (hoprnet#8519 defaults)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile's own arithmetic, restated independently of the constants that produced it.
    ///
    /// Every figure here is a published one — hoprnet#8519's tables, the PIX loss-tolerance
    /// reference's §4.2 and §5 — so this catches a geometry edited without re-deriving what depends
    /// on it.
    #[test]
    fn the_deployed_arithmetic_holds() {
        assert_eq!(454_656, CYCLE_PACKETS, "4096 x (32 + 79)");
        // The published figure is the v5 line's: 4096 x 111 x 1452 B = 629.6 MiB. The v4 line
        // prices a share at the whole HOPR payload instead, so only the relation is common.
        #[cfg(feature = "v5")]
        assert_eq!(660_160_512, QUOTA_PER_SSA, "629.6 MiB per cycle");
        assert_eq!(CYCLE_PACKETS * QUOTA_BYTES_PER_SHARE, QUOTA_PER_SSA);
    }

    /// The surplus is the fewest with which a cycle of these dimensions still recovers at 99 %
    /// when every share is lost with probability one half.
    ///
    /// This is the property `default_surplus_for` implements, restated here from the binomial so a
    /// change to the upstream derivation that moves the value fails *here* rather than silently
    /// moving what this profile exercises. The reference's §4.2 quotes the same configuration at a
    /// 50.3 % tolerated loss.
    #[test]
    fn the_deployed_split_tolerates_half_the_shares() {
        let tolerance = tolerated_loss(PIX_POLYS, PIX_SHARES, PIX_ADDITIONAL_SHARES);
        assert!(
            (0.49..=0.52).contains(&tolerance),
            "4096 x (32 + 79) tolerates {tolerance:.4} of shares lost, not the ~0.50 the deployed \
             split is derived for"
        );
        // One share fewer and it no longer meets the target — the derivation is a *minimum*.
        let one_less = tolerated_loss(PIX_POLYS, PIX_SHARES, PIX_ADDITIONAL_SHARES - 1);
        assert!(
            one_less < 0.50,
            "the surplus is not minimal: {} surplus shares already tolerate {one_less:.4}",
            PIX_ADDITIONAL_SHARES - 1
        );
    }

    /// The per-polynomial failure budget at the deployed confidence, restated.
    #[test]
    fn the_cycle_needs_every_polynomial() {
        // `-ln(0.99)/polys` is the per-polynomial failure budget at 99 % of cycles, to better than
        // 0.001 % across the practical range (reference §2).
        let budget = -(0.99f64.ln()) / PIX_POLYS as f64;
        assert!(
            (2.45e-6..2.46e-6).contains(&budget),
            "the per-polynomial failure budget is {budget:e}, not ~2.454e-6"
        );
    }

    /// The offered quota has to land inside the Exit's accepted window, or every Session is
    /// refused with `UnacceptablePixParams` before a byte moves.
    #[test]
    fn the_quota_fills_the_window_the_exit_accepts() {
        assert_eq!(
            QUOTA_RANGE_MAX, QUOTA_PER_SSA,
            "the top of the window must be exactly one deployed cycle"
        );
        assert_eq!(
            QUOTA_RANGE_MIN * 4,
            QUOTA_RANGE_MAX,
            "the floor is a quarter"
        );
    }

    /// A cycle must outlast a deposit round trip at every rate the scenario will run at, and the
    /// default rate must be one the machine can actually hold.
    #[test]
    fn the_default_rate_leaves_the_cycle_longer_than_a_deposit() {
        let cycle = CYCLE_PACKETS / DEFAULT_PACKET_RATE;
        assert!(
            cycle >= 3 * DEPOSIT_ROUND_TRIP_SECS,
            "a cycle is {cycle} s against a {DEPOSIT_ROUND_TRIP_SECS} s deposit round trip"
        );
        assert!(
            cycle <= MAX_DEPOSIT_WAIT.as_secs() * 4,
            "a cycle of {cycle} s outlasts the Exit's {} s deposit deadline",
            MAX_DEPOSIT_WAIT.as_secs()
        );
    }

    /// The default rate must be one the machine can hold.
    ///
    /// hoprd's own soak measures the flow sustaining 6 000 datagrams/s each way and saturating by
    /// 6 500, and its demo commits to 4 000. This is a separate test rather than an assertion in
    /// the one above because both operands are constants, so the comparison is a compile-time
    /// tautology to clippy — which is exactly the point (a rate raised past the measured ceiling
    /// should fail here), but has to be written where clippy can see it is deliberate.
    #[test]
    fn the_default_rate_is_one_the_machine_sustains() {
        let ceiling = std::hint::black_box(6_000);
        assert!(
            DEFAULT_PACKET_RATE <= ceiling,
            "the default rate {DEFAULT_PACKET_RATE} is above the {ceiling}/s the flow is measured \
             to sustain on this class of machine"
        );
    }

    /// The SURB buffer must stay well under a cycle, and must clear the ring-buffer ceiling the
    /// balancer evicts into.
    ///
    /// The band is 5–15 %, wider than `shapes`' 3–10 %, because the sizing here is hoprd's own
    /// soak formula rather than the traffic-shape profile's: `runway × (share_rate + data_rate)`,
    /// and at this geometry the two rates are equal by construction, so the buffer lands at
    /// `2 × runway / cycle_secs` = 14.1 % rather than the 5.2 % the deployed ratio is quoted at.
    /// A deeper buffer is a longer pipeline between a share being generated and delivered, not a
    /// faster one — the failure mode `shapes::DEFAULT_SURB_BUFFER` documents at length — so the
    /// point of the band is that the buffer stays a fraction of a cycle rather than a multiple of
    /// one.
    #[test]
    fn the_surb_buffer_is_a_small_fraction_of_a_cycle() {
        let ratio = surb_buffer_target() as f64 / CYCLE_PACKETS as f64;
        assert!(
            (0.05..0.15).contains(&ratio),
            "buffer/E is {ratio:.4}, outside the 5-15 % band this sizing lands in (14.1 %)"
        );
        // The free credit a recovered cycle's FIFO tail is credited for — `polys × surplus` — must
        // cover the queue, or the egress gate blocks part-way through the very drain that was
        // clearing it. At this geometry it does so several times over, which is why the gate
        // ceiling can stay at the buffer plus a second of replies.
        const FREE_CREDIT: u64 = (PIX_POLYS * PIX_ADDITIONAL_SHARES) as u64;
        let buffer = surb_buffer_target();
        assert!(
            FREE_CREDIT > buffer,
            "the drain ({buffer}) is no longer covered by the free credit ({FREE_CREDIT}), so \
             max_served_without_progress must rise"
        );
        assert!(
            surb_buffer_target() < SURB_BUFFER_CEILING,
            "the balancer would overshoot into a full ring buffer and evict shares"
        );
    }

    /// The rendered YAML names every field the profile means to move.
    #[test]
    fn the_rendered_yaml_carries_the_geometry() {
        let yaml = cluster_pix_yaml();
        for key in [
            "num_ssa_parts: 4096",
            "ssa_part_size: 32",
            "additional_shares: 79",
            "quota_range_min: 165040128",
            "quota_range_max: 660160512",
            "max_recovery_time: 14400s",
            "max_deposit_wait: 60s",
            "max_ssa_delivery_time: 20s",
            "fill_enabled: true",
            "fill_max_rate: 250",
            "fill_finish_fraction: 0.75",
        ] {
            assert!(yaml.contains(key), "missing `{key}` in:\n{yaml}");
        }
    }

    /// The deposit is the price times the quota, and it has to sit inside the Safe float the
    /// cluster is told to grant — or a run ends on an exhausted float rather than on its target.
    #[test]
    fn the_deposit_is_affordable_from_the_configured_float() {
        let per_cycle = per_cycle_wxhopr();
        assert!(
            (400.0..550.0).contains(&per_cycle),
            "one cycle costs {per_cycle} wxHOPR, outside the ~472 the price implies"
        );
        let float = safe_deposit_float_wxhopr();
        assert!(
            float >= per_cycle * BUDGETED_CYCLES as f64 * 0.999,
            "the float {float} does not cover {BUDGETED_CYCLES} cycles"
        );
        // And the float is what a localcluster identity can be granted: the Safe is funded from a
        // fixed pot, so a float far above it would simply never be reached.
        assert!(
            float <= 20_000.0,
            "the float {float} is larger than the cluster can fund"
        );
    }

    /// `P(Binom(t + s, 1 - L) >= t)^polys >= 0.99`, solved for `L` by bisection.
    ///
    /// A local restatement of the reference's `max_loss`, written in log space so the exponent at
    /// 4096 polynomials does not underflow the way a naive `f64` power of a near-one base does.
    fn tolerated_loss(polys: usize, threshold: usize, surplus: usize) -> f64 {
        let emitted = threshold + surplus;
        // `failure(L)` is the per-polynomial tail `P(Binom(emitted, 1 - L) < threshold)`, and it
        // rises monotonically with L, so the cycle recovers while `-ln(1 - failure) <= target`.
        let failure = |loss: f64| -> f64 {
            if loss <= 0.0 {
                return 0.0;
            }
            if loss >= 1.0 {
                return 1.0;
            }
            let q = 1.0 - loss;
            // Sum the tail in log space; the terms underflow in plain f64 at these sizes.
            let mut ln_choose = 0.0f64;
            let mut peak = f64::NEG_INFINITY;
            let mut ln_terms = Vec::with_capacity(threshold);
            for k in 0..threshold {
                let ln_term = ln_choose + k as f64 * q.ln() + (emitted - k) as f64 * loss.ln();
                peak = peak.max(ln_term);
                ln_terms.push(ln_term);
                ln_choose += ((emitted - k) as f64).ln() - ((k + 1) as f64).ln();
            }
            let log_sum = peak + ln_terms.iter().map(|t| (t - peak).exp()).sum::<f64>().ln();
            log_sum.exp().min(1.0)
        };
        // The cycle recovers while `polys * ln(1 - failure) >= ln(0.99)`; bisect on that.
        let (mut tolerated, mut fatal) = (0.0f64, 1.0f64);
        for _ in 0..60 {
            let mid = 0.5 * (tolerated + fatal);
            let f = failure(mid);
            let recovers = polys as f64 * (-f).ln_1p() >= 0.99f64.ln();
            if recovers {
                tolerated = mid;
            } else {
                fatal = mid;
            }
        }
        tolerated
    }
}
