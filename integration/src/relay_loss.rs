//! The PIX geometry and Session sizing for surviving the death of half the return relayers.
//!
//! [`crate::shapes`] asks whether a cycle completes under the traffic an application produces.
//! This asks whether it completes when **half the relays carrying its shares home disappear**, and
//! the two need different parameters for a reason that is entirely about PIX: a cycle is only ever
//! worth anything whole. An SSA is the sum of *every* polynomial's constant term, so a cycle that
//! loses one polynomial pays nothing at all, and the deposit — whose address derives from both
//! sides' commitments — is stranded rather than refunded.
//!
//! # What a relayer's death actually costs
//!
//! Three distinct losses, and only the first is the obvious one. Entry `E`, relays `R` (which
//! dies) and `S`, Exit `X`; return paths are drawn independently per SURB, so each relay carries
//! about half the stream.
//!
//! 1. **SURBs already at the Exit that name `R`.** A share is bound to its SURB when the SURB is
//!    *minted*, and it only reaches the reconstructor when the first relayer acknowledges the
//!    packet carrying it. Spending one through a dead `R` reveals nothing: the share is gone. Half
//!    the Exit's buffer is in this state the instant `R` dies.
//! 2. **The other half of the Exit's buffer.** This is the one that does not follow from anything
//!    PIX documents. The Entry notices the silence, re-plans, and bumps its SURB *generation*; the
//!    Exit's `SurbRingBuffer::push` then clears **both tiers wholesale** on the first batch
//!    carrying the newer generation. Return paths are drawn per SURB, so the buffer was never "all
//!    for the superseded path" — about half of it named the surviving `S` and would have delivered.
//!    Those shares are dropped too, and they are not counted in `evicted_shares`, which counts
//!    capacity overflow only.
//! 3. **Everything spent into the void while the silence is being detected.** The detector needs
//!    five consecutive one-second flushes with no reply over a pair of legs, corroborated by a
//!    sibling pair that *is* delivering. Until it fires, about half of what the Exit sends is
//!    addressed to a corpse.
//!
//! So, with `B` the Exit's SURB holding, `R_e` its egress rate and `T_d` the detection window:
//!
//! ```text
//! lost  ≈  B  +  ½ · R_e · T_d
//! ```
//!
//! # What absorbs it
//!
//! Not `surplus / threshold`. A polynomial reconstructs from the first `threshold` distinct shares
//! to arrive out of `threshold + surplus` emitted, so the loss *rate* it survives is
//! `surplus / (threshold + surplus)` — 20 % at the shipped 64 + 16, never 25 %. That ratio is the
//! wrong instrument here anyway, because this loss is not a rate: it is one burst.
//!
//! What matters for a burst is that emission is **round-robin over
//! [`hopr_protocol_pix::SHARE_EMISSION_WINDOW`] = 256 polynomials advancing in lockstep**. A
//! contiguous run of `L` lost shares is therefore spread across 256 polynomials, costing each of
//! them `L / 256`, and each can afford to lose exactly `surplus`:
//!
//! ```text
//! tolerated burst  =  256 · surplus
//! ```
//!
//! That is the whole design rule, and [`burst_margin`] is it stated as a ratio:
//!
//! ```text
//! 256 · surplus  ≥  margin · (B + ½ · R_e · T_d)
//! ```
//!
//! # The second parameter, which is not a PIX one
//!
//! Surviving the burst is necessary and not sufficient, because after the wipe the Exit holds
//! nothing and the Entry does not know it. The balancer estimates the counterparty's buffer as
//! `produced − consumed`, and consumption is only observed when a reply gets home — so a return
//! path that drops everything reads exactly like a counterparty that is well stocked. hoprnet#8490
//! pins that behaviour in a unit test: the estimate sits at or above target and the controller
//! winds production down to zero, precisely while the Exit is emptying.
//!
//! The client's opt-in for that, `sustain_on_return_path_loss` (GNO-713), is deliberately neither
//! set nor named here: hoprnet#8506 removes the field and replaces it with a write-off of the SURBs
//! through a relayer judged dead, and hoprnet#8576 has the Entry reset the balancer and pace its
//! refill when the SURB generation advances. [`surb_balancer`] sets only the two fields the client
//! does, so the suite builds on both sides of that change.
//!
//! # Which stack the mechanism below describes
//!
//! The loss model is written for the stack of 2026-10-17: the Entry's silence detector decides a
//! relayer is gone and its re-plan advances the SURB generation. hoprnet#8506 replaces that detector
//! with a dead verdict from SURB reply races, and #8576 makes the wipe that follows deterministic.
//! The burst is the same one either way (the Exit's whole stock plus the spend before detection),
//! which is what the design rule is stated against. `docs/pix-relay-loss.md` has the measurements
//! on both stacks.
//!
//! # Overrides
//!
//! [`additional_shares`] reads `HOPRD_PIX_ADDITIONAL_SHARES`, so the control run — the shipped
//! surplus, which this profile predicts is too thin — needs no recompile. At the default
//! dimensions the accepted quota window is sized to hold the whole `8..=64` sweep, so the Exit's
//! admission policy does not move between runs and a failure can never be an admission failure
//! wearing a disguise. `HOPRD_PIX_POLYS`, `HOPRD_PIX_THRESHOLD` and `HOPRD_PIX_RATE` move the whole
//! geometry to production's (4096 polynomials, threshold 32), and `HOPRD_PIX_SURB_BUFFER` the
//! Exit's holding; the window follows them.

use std::time::Duration;

use edgli::hopr_lib::exports::transport::{SESSION_MTU, SURB_SIZE};

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Polynomials per SSA.
///
/// A whole number of emission windows (512 = 2 x 256), so every block the generator walks is
/// full-width and a burst is spread over the full 256. A count that is not a multiple narrows the
/// last window, which concentrates the same burst onto fewer polynomials — the one geometry choice
/// here that changes the tolerance rather than merely the duration.
pub const PIX_POLYS: usize = 512;

/// Shares needed to reconstruct one polynomial — the deployed threshold, unchanged.
pub const PIX_SHARES: usize = 64;

/// Shares emitted beyond the threshold, per polynomial, before [`additional_shares`] applies its
/// override.
///
/// Twice what upstream derives when `additional_shares` is left unset, and chosen by the
/// inequality in the module docs rather than by doubling: `256 x 32` = 8192 against an expected
/// burst of about 3000 at this profile's buffer and rate, i.e. [`burst_margin`] ≈ 2.7.
///
/// Not larger, for two reasons that both cost money. The surplus is billed on purchase — it rides
/// in the negotiated `PixParams` and the per-SSA quota counts it — so every extra share is paid
/// for in every deposit whether or not anything is lost. And `threshold + surplus` should stay
/// inside one peer deferral bucket (`MAX_DEFERRED_ACKS_PER_POLYNOMIAL`, 128), which 96 does.
pub const DEFAULT_ADDITIONAL_SHARES: usize = 32;

/// Smallest surplus the sweep accepts.
///
/// Below this the quota leaves the window [`QUOTA_RANGE_MIN`] opens, and the Session would be
/// refused with `UnacceptablePixParams` before a byte moved — a failure that looks nothing like
/// the one under test.
pub const MIN_ADDITIONAL_SHARES: usize = 8;

/// Largest surplus of the default-dimension sweep: the threshold.
///
/// It sizes the quota window at the default dimensions, [`QUOTA_RANGE_MAX_SHARES`]. It is no upstream
/// limit: hoprnet#8519 lifted the bound on `additional_shares` by `ssa_part_size`, and production
/// ships a surplus of 79 against a threshold of 32. A run that asks for more than this stays
/// admissible because [`quota_range_max_shares`] follows the surplus.
pub const MAX_ADDITIONAL_SHARES: usize = PIX_SHARES;

/// Polynomials the generator advances in lockstep, from `hopr-protocol-pix`.
///
/// Restated rather than imported because the constant is not re-exported through edgli's
/// dependency surface, and [`the_window_matches_upstream`](tests::the_window_matches_upstream)
/// documents how to re-check it. It is the whole reason a burst is survivable at all: it is the
/// divisor in `tolerated burst = window x surplus`.
pub const SHARE_EMISSION_WINDOW: u64 = 256;

/// Return packets per second the scenario offers, and therefore the Exit's egress rate.
///
/// [`crate::shapes::TARGET_PACKET_RATE`], unchanged, so the two profiles' cycle lengths are
/// comparable. It also has to clear the detector's own floor: silence is only judged on a pair of
/// legs that minted at least 20 SURBs in a one-second flush, and with two forward legs and two
/// reply legs there are four pairs to keep above it.
pub const TARGET_PACKET_RATE: u64 = 150;

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Polynomials per SSA for this run: `HOPRD_PIX_POLYS`, else [`PIX_POLYS`]. Production ships 4096.
pub fn pix_polys() -> usize {
    env_or("HOPRD_PIX_POLYS", PIX_POLYS)
}

/// Threshold shares per polynomial: `HOPRD_PIX_THRESHOLD`, else [`PIX_SHARES`]. Production ships 32.
pub fn pix_shares() -> usize {
    env_or("HOPRD_PIX_THRESHOLD", PIX_SHARES)
}

/// Offered packet rate: `HOPRD_PIX_RATE`, else [`TARGET_PACKET_RATE`]. Production dimensions make a
/// cycle of 454 656 packets, which at 150 pkt/s is 50 min; a local run raises the rate instead.
pub fn target_packet_rate() -> u64 {
    env_or("HOPRD_PIX_RATE", TARGET_PACKET_RATE as usize).max(1) as u64
}

/// Entry-side SURB mint ceiling for this run, four times the offered rate.
pub fn max_surbs_per_sec() -> u64 {
    4 * target_packet_rate()
}

/// The quota window the Exit accepts, in shares and bytes, for this run's dimensions. Its top admits
/// the larger of the threshold and the surplus actually asked for: production's surplus (79) exceeds
/// its threshold (32) since hoprnet#8519 lifted the ratio bound.
pub fn quota_range_min_shares() -> u64 {
    (pix_polys() * (pix_shares() + MIN_ADDITIONAL_SHARES)) as u64
}
pub fn quota_range_max_shares() -> u64 {
    (pix_polys() * (pix_shares() + additional_shares().max(pix_shares()))) as u64
}
pub fn quota_range_min() -> u64 {
    quota_range_min_shares() * QUOTA_BYTES_PER_SHARE
}
pub fn quota_range_max() -> u64 {
    quota_range_max_shares() * QUOTA_BYTES_PER_SHARE
}

/// The Exit's recovery deadline for this run: [`MAX_RECOVERY_TIME`], or twice the nominal cycle when
/// that is longer, so a production-sized cycle at a local rate is not closed before it can finish.
pub fn max_recovery_time() -> Duration {
    MAX_RECOVERY_TIME.max(Duration::from_secs(2 * nominal_cycle_secs()))
}

/// Fill ceiling for this run: [`FILL_MAX_RATE`], or what the Exit's own validator demands when the
/// geometry is larger -- fill must be able to finish the widest accepted quota within
/// `max_recovery_time x fill_finish_fraction` (`validate_incoming_session_pix_config`), else the node
/// refuses the config and never starts. 5 % above that floor, as the profile's unit test demands.
pub fn fill_max_rate() -> u32 {
    let horizon = max_recovery_time().as_secs_f64() * FILL_FINISH_FRACTION;
    let floor = (quota_range_max_shares() as f64 * 1.05 / horizon).ceil() as u32;
    FILL_MAX_RATE.max(floor)
}

/// Bytes of quota one share is priced at, as the Exit computes it.
#[cfg(feature = "v5")]
pub const QUOTA_BYTES_PER_SHARE: u64 =
    edgli::hopr_lib::exports::transport::session::PIX_QUOTA_BYTES_PER_SHARE;
#[cfg(not(feature = "v5"))]
pub const QUOTA_BYTES_PER_SHARE: u64 =
    edgli::hopr_lib::exports::transport::PACKET_PAYLOAD_SIZE as u64;

/// Surplus shares per polynomial, honouring `HOPRD_PIX_ADDITIONAL_SHARES`.
///
/// Clamped to `MIN_ADDITIONAL_SHARES..=u8::MAX` rather than rejected, so a sweep script cannot put
/// the run outside the Exit's admission window by typing a number. The ceiling is the field's
/// type, not [`MAX_ADDITIONAL_SHARES`]: production's surplus (79) exceeds its threshold since
/// hoprnet#8519, and [`quota_range_max_shares`] widens the window to hold whatever is asked for.
pub fn additional_shares() -> usize {
    std::env::var("HOPRD_PIX_ADDITIONAL_SHARES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ADDITIONAL_SHARES)
        .clamp(MIN_ADDITIONAL_SHARES, u8::MAX as usize)
}

/// `E` — every share one cycle emits, and therefore every packet it takes to complete one.
pub fn cycle_packets() -> u64 {
    (pix_polys() * (pix_shares() + additional_shares())) as u64
}

/// Bytes one cycle costs the Entry — the per-SSA quota the Exit prices its deposit against.
pub fn quota_per_ssa() -> u64 {
    cycle_packets() * QUOTA_BYTES_PER_SHARE
}

/// Nominal seconds per cycle, `E / R`. The achieved figure runs longer; see
/// [`crate::shapes::NOMINAL_CYCLE_SECS`].
pub fn nominal_cycle_secs() -> u64 {
    cycle_packets() / target_packet_rate()
}

// ── The loss model ───────────────────────────────────────────────────────────

/// Seconds of silence the Entry needs before it re-plans and supersedes the Exit's SURBs.
///
/// `SILENT_FLUSHES_BEFORE_DEGRADED` (5) flushes at `surb_flush_interval` (1 s), plus a margin for
/// the re-plan, the generation bump and the first superseding batch reaching the Exit. Upstream
/// constants, neither of them configurable, so this is an estimate of their sum rather than a
/// setting — which is why [`BURST_MARGIN_FLOOR`] exists rather than a prediction being asserted.
pub const DETECTION_SECS: u64 = 8;

/// Shares a relayer's death is expected to cost, at a given Exit SURB holding.
///
/// `B + ½ · R · T_d` — see the module docs. An estimate of a burst, deliberately: the point of
/// the profile is that the tolerance clears it several times over, not that the figure is exact.
pub fn expected_burst(surb_buffer: u64) -> u64 {
    surb_buffer + target_packet_rate() * DETECTION_SECS / 2
}

/// Contiguous lost shares one cycle can absorb: `window x surplus`.
pub fn tolerated_burst() -> u64 {
    SHARE_EMISSION_WINDOW * additional_shares() as u64
}

/// How many times over the tolerance clears the expected burst.
///
/// The profile's one headline number. Below 1.0 the cycle is predicted to die; the scenario asserts
/// nothing against it, because the prediction is what the run is there to test.
pub fn burst_margin() -> f64 {
    tolerated_burst() as f64 / expected_burst(surb_buffer_target()).max(1) as f64
}

/// Surplus an Exit SURB holding of `surb_buffer` demands to clear its burst by `margin`.
///
/// The design rule solved for the surplus: `surplus ≥ margin · burst / window`. The inverse of
/// [`burst_margin`], and the form an operator sizing a deployment needs — the buffer is chosen for
/// the service and the surplus then has to follow it.
pub fn required_surplus(surb_buffer: u64, margin: f64) -> u64 {
    (margin * expected_burst(surb_buffer) as f64 / SHARE_EMISSION_WINDOW as f64).ceil() as u64
}

/// SURBs a deployed Exit holds, from `docs/pix-traffic-shapes.md`'s deployed column.
///
/// Here so that [`the_deployed_buffer_is_covered_by_the_production_surplus_with_three_shares_to_spare`](tests::the_deployed_buffer_is_covered_by_the_production_surplus_with_three_shares_to_spare)
/// can state what this profile's arithmetic implies about a real deployment, which is not
/// something a three-node cluster can measure.
pub const DEPLOYED_SURB_BUFFER: u64 = 19_264;

/// The surplus production ships, per polynomial (4096 x (32 + 79)), since hoprnet#8519 lifted the
/// bound by the threshold.
pub const PRODUCTION_ADDITIONAL_SHARES: u64 = 79;

/// Margin the shipped configuration is expected to fall below, and this profile to clear.
///
/// Stated so the log line can say which side of it a run sits on without the reader doing the
/// arithmetic. Two rather than one because the burst is an estimate built on three upstream
/// constants, and a configuration that only just clears an estimate has not been shown to clear
/// anything.
pub const BURST_MARGIN_FLOOR: f64 = 2.0;

// ── Exit-side deadlines ──────────────────────────────────────────────────────

/// Exit's deadline for the SSA commitment to arrive. [`crate::shapes::MAX_SSA_DELIVERY_TIME`].
pub const MAX_SSA_DELIVERY_TIME: Duration = Duration::from_secs(20);

/// Exit's deadline for the deposit to land. [`crate::shapes::MAX_DEPOSIT_WAIT`].
pub const MAX_DEPOSIT_WAIT: Duration = Duration::from_secs(30);

/// Absolute per-cycle recovery deadline.
///
/// [`crate::shapes::MAX_RECOVERY_TIME`]'s fifteen minutes, which clears the load-time floor and is
/// a wait a test can afford. It is also what bounds a *failure* here: a cycle whose polynomials
/// have been starved does not announce itself, it simply stops advancing, and what ends the
/// Session is `max_recovery_idle` once the Entry's generator has emitted the last share it has —
/// or this, if anything keeps trickling.
pub const MAX_RECOVERY_TIME: Duration = Duration::from_secs(900);

/// Fraction of [`MAX_RECOVERY_TIME`] at which Exit fill aims to have the cycle finished.
pub const FILL_FINISH_FRACTION: f64 = 0.6;

/// How long a cycle may go without *any* share arriving before the Exit closes the Session.
///
/// Upstream's default, left alone — restated here because it is what bounds how long a *failure*
/// takes to surface, and the scenario's budget has to cover it. A cycle whose polynomials were
/// starved does not announce itself: the Entry goes on emitting until every polynomial has spent
/// its `threshold + surplus`, and only then does the share stream stop and this fire. So a run
/// that is going to fail does so roughly one minute after the cycle's emission runs dry, not at
/// the moment the damage is done.
pub const MAX_RECOVERY_IDLE: Duration = Duration::from_secs(60);

/// Ceiling on the Exit's self-generated fill traffic, packets/s.
///
/// Upstream's default, and it has to clear what a cycle of [`QUOTA_RANGE_MAX`] needs inside the aim
/// point or `validate_incoming_session_pix_config` refuses the configuration at load — see
/// [`the_fill_ceiling_clears_the_widest_quota`](tests::the_fill_ceiling_clears_the_widest_quota).
pub const FILL_MAX_RATE: u32 = 250;

// ── Admission window ─────────────────────────────────────────────────────────

/// Lower bound of the quota window the Exit accepts, in shares.
///
/// Sized to hold the *whole* `MIN_ADDITIONAL_SHARES..=MAX_ADDITIONAL_SHARES` sweep, so that the
/// control run and the candidate run face one admission policy. A window that moved with the
/// surplus would make a refused Session — which arrives here as a Session-open timeout — one of the
/// ways this scenario could appear to fail.
pub const QUOTA_RANGE_MIN_SHARES: u64 = (PIX_POLYS * (PIX_SHARES + MIN_ADDITIONAL_SHARES)) as u64;

/// Upper bound of the quota window the Exit accepts, in shares. See [`QUOTA_RANGE_MIN_SHARES`].
pub const QUOTA_RANGE_MAX_SHARES: u64 = (PIX_POLYS * (PIX_SHARES + MAX_ADDITIONAL_SHARES)) as u64;

/// Lower bound of the accepted quota, in bytes.
pub const QUOTA_RANGE_MIN: u64 = QUOTA_RANGE_MIN_SHARES * QUOTA_BYTES_PER_SHARE;

/// Upper bound of the accepted quota, in bytes.
pub const QUOTA_RANGE_MAX: u64 = QUOTA_RANGE_MAX_SHARES * QUOTA_BYTES_PER_SHARE;

// ── Session sizing (Entry side) ──────────────────────────────────────────────

/// SURBs the Entry keeps buffered at the Exit, before any override.
///
/// `16 x R`, [`crate::shapes::DEFAULT_SURB_BUFFER`]'s rule, which puts the buffer at the ~5 % of a
/// cycle the deployed profile runs at.
///
/// **It is also the dominant term in the burst**, which is the tension this profile has to hold.
/// A shallower buffer would make a relayer's death cheaper — it is the quantity the generation
/// bump throws away — but it is also the Exit's whole runway, and a buffer sized for the failure
/// rather than for the service measures a Session nobody would deploy. So the depth stays
/// realistic and the *surplus* is what pays for the failure.
pub const DEFAULT_SURB_BUFFER: u64 = 16 * TARGET_PACKET_RATE;

/// Ceiling on SURB production, per second.
///
/// `4 x R`. Load-bearing here in a way it is not for a steady shape: after the wipe the Exit holds
/// nothing, and this is the rate at which the open-loop refill replaces it. It has to put a whole
/// buffer back inside the degraded-path grace (10 s, an upstream constant), or the Session
/// recovers its route and then starves anyway — see
/// [`the_refill_rate_beats_the_degraded_grace`](tests::the_refill_rate_beats_the_degraded_grace).
pub const MAX_SURBS_PER_SEC: u64 = 4 * TARGET_PACKET_RATE;

/// How long an outside observer's degraded-path mark keeps the balancer in open loop.
///
/// `RETURN_PATH_DEGRADED_GRACE` in `hopr-transport`, which is a private constant rather than a
/// setting — restated so the refill guard below has something to check against.
pub const RETURN_PATH_DEGRADED_GRACE: Duration = Duration::from_secs(10);

/// SURBs the Entry buffers at the Exit, honouring `HOPRD_PIX_SURB_BUFFER`.
pub fn surb_buffer_target() -> u64 {
    std::env::var("HOPRD_PIX_SURB_BUFFER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16 * target_packet_rate())
}

/// Shares a recovered cycle's FIFO tail is credited for — `parts x surplus`.
pub fn free_credit() -> u64 {
    (pix_polys() * additional_shares()) as u64
}

/// Packets the Exit serves without a share coming back before its egress gate blocks.
///
/// Upstream's 2048 while the free credit covers the queue, which at every surplus in the sweep it
/// does. [`crate::shapes::max_served_without_progress`] carries the derivation.
pub fn max_served_without_progress() -> u64 {
    let default = if surb_buffer_target() > free_credit() {
        surb_buffer_target() - free_credit() + target_packet_rate()
    } else {
        2048
    };
    std::env::var("HOPRD_PIX_MAX_SERVED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ── Compile-time guards ──────────────────────────────────────────────────────

/// The window must divide the polynomial count, or a burst lands on a narrower final block and the
/// tolerance this whole profile is built on does not hold there.
const _: () = assert!(
    (PIX_POLYS as u64).is_multiple_of(SHARE_EMISSION_WINDOW),
    "polynomials per SSA must be a whole number of emission windows"
);

/// `additional_shares()` clamps to `u8::MAX`, so the sweep's own ceiling has to be a value that
/// clamp admits. (Upstream no longer bounds the surplus by the threshold: hoprnet#8519.)
const _: () = assert!(
    MAX_ADDITIONAL_SHARES <= u8::MAX as usize,
    "the sweep's largest surplus must survive the clamp in additional_shares()"
);

/// A polynomial's whole emission should fit one peer deferral bucket
/// (`MAX_DEFERRED_ACKS_PER_POLYNOMIAL`, 128), or acknowledgements arriving before the cycle's
/// commitments install can be dropped.
const _: () = assert!(
    PIX_SHARES + DEFAULT_ADDITIONAL_SHARES <= 128,
    "threshold + surplus must fit one deferral bucket at the default surplus"
);

// ── Rendering ────────────────────────────────────────────────────────────────

/// wxHOPR per byte of quota, on both sides. [`crate::shapes::PRICE_PER_BYTE_WXHOPR`].
pub const PRICE_PER_BYTE_WXHOPR: f64 = 1e-9;

/// Cycles a scenario's budget covers. Well above what this scenario completes.
pub const BUDGETED_CYCLES: u64 = 12;

/// This profile as the YAML `cluster::request_pix_settings` passes to `--pix-config`.
pub fn cluster_pix_yaml() -> String {
    let per_cycle_wxhopr = quota_per_ssa() as f64 * PRICE_PER_BYTE_WXHOPR;
    // Priced against the widest quota the window admits rather than this run's, so the per-deposit
    // ceiling does not have to move when the surplus knob does.
    let widest_wxhopr = quota_range_max() as f64 * PRICE_PER_BYTE_WXHOPR;
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
fill_enabled: true
fill_max_rate: {fill_rate}
fill_finish_fraction: {fill_finish}
price_per_byte: \"{price:.10} wxHOPR\"
max_ssa_allocation: \"{allocation:.4} wxHOPR\"
max_spend_per_window: \"{budget:.4} wxHOPR\"
safe_deposit_float: \"{float:.4} wxHOPR\"
",
        polys = pix_polys(),
        shares = pix_shares(),
        surplus = additional_shares(),
        quota_min = quota_range_min(),
        quota_max = quota_range_max(),
        delivery = MAX_SSA_DELIVERY_TIME.as_secs(),
        deposit = MAX_DEPOSIT_WAIT.as_secs(),
        recovery = max_recovery_time().as_secs(),
        served = max_served_without_progress(),
        fill_rate = fill_max_rate(),
        fill_finish = FILL_FINISH_FRACTION,
        price = PRICE_PER_BYTE_WXHOPR,
        allocation = widest_wxhopr * 2.0,
        budget = per_cycle_wxhopr * BUDGETED_CYCLES as f64,
        float = per_cycle_wxhopr * BUDGETED_CYCLES as f64,
    )
}

/// This profile's SURB balancer: the two fields the client sets, and the defaults for the rest.
pub fn surb_balancer() -> edgli::hopr_lib::exports::transport::SurbBalancerConfig {
    edgli::hopr_lib::exports::transport::SurbBalancerConfig {
        target_surb_buffer_size: surb_buffer_target(),
        max_surbs_per_sec: max_surbs_per_sec(),
        // The client also sets `sustain_on_return_path_loss` (GNO-713). hoprnet#8506 removes that
        // field, replacing it with the write-off of SURBs through a dead relayer, so it is not
        // named here: the suite then compiles on both sides of that change.
        ..Default::default()
    }
}

/// Bytes one SURB buffer of the requested depth occupies.
pub fn surb_buffer_bytes() -> u64 {
    surb_buffer_target() * SESSION_MTU as u64
}

/// Bits per second of SURB upstream the balancer may use.
pub const MAX_SURB_UPSTREAM_BITS: u64 = MAX_SURBS_PER_SEC * 8 * SURB_SIZE as u64;

// ── Entry side ───────────────────────────────────────────────────────────────

/// This profile as the Entry's own generator dimensions.
#[cfg(feature = "v5")]
pub fn entry_dimensions() -> edgli::PixGlobalConfig {
    edgli::PixGlobalConfig {
        num_ssa_parts: pix_polys(),
        ssa_part_size: pix_shares(),
        additional_shares: Some(additional_shares()),
        ..Default::default()
    }
}

/// wxHOPR one completed cycle costs the Entry at this profile.
#[cfg(feature = "v5")]
pub fn per_cycle() -> anyhow::Result<crate::HoprBalance> {
    let price: crate::HoprBalance = format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR").parse()?;
    Ok(price * quota_per_ssa())
}

/// The Entry's settlement configuration for this profile.
#[cfg(feature = "v5")]
pub fn entry_config() -> anyhow::Result<edgli::PixEntryConfig> {
    let per_cycle = per_cycle()?;
    // Against the widest admitted quota, matching `cluster_pix_yaml`: the two ceilings are read by
    // different processes and a run whose surplus moved one but not the other would refuse its own
    // deposits.
    let widest: crate::HoprBalance = format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR").parse()?;
    let widest = widest * quota_range_max();
    Ok(edgli::PixEntryConfig {
        strategy: edgli::PixEntryStrategy {
            price_per_byte: format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR").parse()?,
            max_ssa_allocation: widest * 2u64,
            max_spend_per_window: per_cycle * BUDGETED_CYCLES,
            spend_window: crate::pix::SPEND_WINDOW,
            ..Default::default()
        },
        pool: edgli::PixEntryPool {
            max_deposit_tracking_time: Duration::from_secs(40),
            ..Default::default()
        },
        state_dir: None,
    })
}

/// Point both halves of the run at this profile, before the cluster is brought up.
#[cfg(feature = "v5")]
pub fn install_profile() {
    crate::cluster::request_pix_settings(cluster_pix_yaml());
    crate::pix::request_dimensions(entry_dimensions());
    tracing::info!(
        polys = pix_polys(),
        shares = pix_shares(),
        surplus = additional_shares(),
        rate = target_packet_rate(),
        max_recovery_secs = max_recovery_time().as_secs(),
        cycle_packets = cycle_packets(),
        quota = quota_per_ssa(),
        nominal_cycle_secs = nominal_cycle_secs(),
        surb_buffer = surb_buffer_target(),
        expected_burst = expected_burst(surb_buffer_target()),
        tolerated_burst = tolerated_burst(),
        burst_margin = format!("{:.2}", burst_margin()),
        "installed the PIX relay-loss profile"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard against `HOPRD_PIX_*` left set in the shell, which would make every expectation below
    /// about a geometry the test did not choose.
    fn at_default_surplus() -> bool {
        [
            "HOPRD_PIX_ADDITIONAL_SHARES",
            "HOPRD_PIX_SURB_BUFFER",
            "HOPRD_PIX_POLYS",
            "HOPRD_PIX_THRESHOLD",
            "HOPRD_PIX_RATE",
        ]
        .iter()
        .all(|k| std::env::var(k).is_err())
    }

    /// The profile's arithmetic, restated independently of the constants that produced it.
    #[test]
    fn the_profile_arithmetic_holds() {
        if !at_default_surplus() {
            return;
        }
        assert_eq!(49_152, cycle_packets());
        assert_eq!(2_400, surb_buffer_target());
        assert_eq!(16_384, free_credit());
        assert_eq!(327, nominal_cycle_secs());
        #[cfg(feature = "v5")]
        assert_eq!(71_368_704, quota_per_ssa());
    }

    /// The headline claim: the tolerance clears the expected burst by the stated margin, and the
    /// shipped surplus does not.
    ///
    /// This is the one test that would have to change if the recommendation changed, which is why
    /// it names both sides. It asserts the *arithmetic*, not the outcome — whether the arithmetic
    /// predicts the cluster is what `tests/pix_relay_loss.rs` is for.
    #[test]
    fn the_surplus_clears_the_burst_and_the_shipped_one_does_not() {
        if !at_default_surplus() {
            return;
        }
        let burst = expected_burst(surb_buffer_target());
        assert!(
            burst_margin() >= BURST_MARGIN_FLOOR,
            "surplus {} tolerates {} contiguous lost shares against an expected burst of {burst} \
             — a margin of {:.2}, below the {BURST_MARGIN_FLOOR} this profile is chosen for",
            additional_shares(),
            tolerated_burst(),
            burst_margin(),
        );

        // The shipped surplus, evaluated by hand against the same burst. `default_surplus_for`
        // derives 16 at threshold 64.
        let shipped_tolerance = SHARE_EMISSION_WINDOW * 16;
        assert!(
            (shipped_tolerance as f64 / burst as f64) < BURST_MARGIN_FLOOR,
            "the shipped surplus of 16 tolerates {shipped_tolerance} against a burst of {burst}, \
             which already clears the floor — the premise of this profile is gone and the control \
             run has nothing to show"
        );
    }

    /// Every surplus the sweep admits must land inside the one quota window the Exit is given, or
    /// a run fails at admission rather than at the thing under test.
    #[test]
    fn every_surplus_in_the_sweep_is_admissible() {
        for surplus in MIN_ADDITIONAL_SHARES..=MAX_ADDITIONAL_SHARES {
            let shares = (PIX_POLYS * (PIX_SHARES + surplus)) as u64;
            assert!(
                (QUOTA_RANGE_MIN_SHARES..=QUOTA_RANGE_MAX_SHARES).contains(&shares),
                "surplus {surplus} implies {shares} shares, outside the accepted window \
                 {QUOTA_RANGE_MIN_SHARES}..={QUOTA_RANGE_MAX_SHARES}"
            );
        }
    }

    /// Fill must be able to finish a cycle of the *widest* accepted quota inside the aim point, or
    /// `validate_incoming_session_pix_config` refuses this configuration at load — before any
    /// Session is opened, which reads here as a cluster that will not start.
    #[test]
    fn the_fill_ceiling_clears_the_widest_quota() {
        let aim_point = MAX_RECOVERY_TIME.as_secs_f64() * FILL_FINISH_FRACTION;
        let widest = QUOTA_RANGE_MAX_SHARES as f64 * 1.05 / aim_point;
        assert!(
            widest <= FILL_MAX_RATE as f64,
            "the widest accepted quota needs {widest:.0} packets/s against a ceiling of \
             {FILL_MAX_RATE}"
        );

        // And this run's own cycle, which is the one that actually has to finish.
        if at_default_surplus() {
            let needed = cycle_packets() as f64 * 1.05 / aim_point;
            assert!(
                needed <= FILL_MAX_RATE as f64,
                "an idle cycle needs {needed:.0} packets/s against {FILL_MAX_RATE}"
            );
        }
    }

    /// The open-loop refill has to replace a whole buffer inside the degraded-path grace.
    ///
    /// Otherwise the Session survives the burst, re-plans onto the surviving relay, and then sits
    /// empty — the estimate is trusted again the moment the grace lapses, and it is still pinned at
    /// the level the wipe never told it about.
    #[test]
    fn the_refill_rate_beats_the_degraded_grace() {
        let refilled = MAX_SURBS_PER_SEC * RETURN_PATH_DEGRADED_GRACE.as_secs();
        assert!(
            refilled >= surb_buffer_target(),
            "the open loop replaces {refilled} SURBs in the {:?} grace, short of the {} the wipe \
             discards",
            RETURN_PATH_DEGRADED_GRACE,
            surb_buffer_target()
        );
    }

    /// The free credit must cover the queue, so `max_served_without_progress` stays at upstream's
    /// default and the egress gate cannot park part-way through a recovered cycle's drain.
    #[test]
    fn the_free_credit_covers_the_drain() {
        if !at_default_surplus() {
            return;
        }
        assert!(free_credit() > surb_buffer_target());
        assert_eq!(2048, max_served_without_progress());
    }

    /// **A deployed Exit's whole SURB buffer is covered by production's surplus, by three shares.**
    ///
    /// The tolerance is `256 · surplus`. Since hoprnet#8519 lifted the bound on the surplus by the
    /// threshold, production ships 79 against a threshold of 32, so it tolerates `256 × 79` = 20 224
    /// lost shares, and a deployed Exit holds ~19 264 SURBs, all of which a generation advance
    /// discards. Per polynomial that is 19 264 / 256 = 75.25, so 76 shares against a surplus of 79:
    /// a margin of **three**.
    ///
    /// Three is the whole point, not a comfortable figure. The wipe alone fits; the model's
    /// detection term does not (`½ · R_e · T_d` at 400 packets/s over the profile's 8 s is another
    /// 1 600 shares), so the production-dimension run that survived did so because the dead verdict
    /// landed within a second, not the eight the model budgets for. A margin that thin is why the
    /// Exit's stock has to be kept near its target rather than let up to its capacity (hoprnet#8576).
    ///
    /// Derived, not measured here — a three-node local cluster cannot hold a deployed buffer; the
    /// measured runs are in `docs/pix-relay-loss.md`. Written as a test so the claim is checked
    /// against the same constants the rest of the profile uses and cannot quietly drift away.
    #[test]
    fn the_deployed_buffer_is_covered_by_the_production_surplus_with_three_shares_to_spare() {
        let tolerated = SHARE_EMISSION_WINDOW * PRODUCTION_ADDITIONAL_SHARES;
        assert_eq!(20_224, tolerated);
        assert!(
            DEPLOYED_SURB_BUFFER <= tolerated,
            "a deployed buffer of {DEPLOYED_SURB_BUFFER} outruns the {tolerated} that surplus \
             {PRODUCTION_ADDITIONAL_SHARES} tolerates — the wipe alone would strand the cycle"
        );

        let per_polynomial = DEPLOYED_SURB_BUFFER.div_ceil(SHARE_EMISSION_WINDOW);
        assert_eq!(76, per_polynomial);
        assert_eq!(
            3,
            PRODUCTION_ADDITIONAL_SHARES - per_polynomial,
            "the margin between the shipped surplus and what a deployed buffer's wipe costs each \
             polynomial is the number the docs quote"
        );

        // The model's own detection term does not fit in the margin, at production's 400 packets/s.
        let detection = 400 * DETECTION_SECS / 2;
        assert!(
            DEPLOYED_SURB_BUFFER + detection > tolerated,
            "with the model's {detection}-share detection term the burst no longer exceeds what \
             production tolerates, so the docs' caveat about detection time can go"
        );
    }

    /// The emission window is restated from `hopr-protocol-pix` rather than imported, so this says
    /// how to re-check it.
    ///
    /// `rg 'pub const SHARE_EMISSION_WINDOW' protocols/pix/src/generator.rs` in a hoprnet checkout.
    /// If it moves, [`tolerated_burst`] moves with it and this whole profile has to be re-derived —
    /// the surplus is sized against the divisor.
    #[test]
    fn the_window_matches_upstream() {
        assert_eq!(
            256, SHARE_EMISSION_WINDOW,
            "hopr-protocol-pix's SHARE_EMISSION_WINDOW is the divisor in `tolerated burst = \
             window x surplus`; re-derive the surplus if it has changed"
        );
    }

    /// The rendered YAML names every field the profile means to move.
    #[test]
    fn the_rendered_yaml_carries_the_geometry() {
        if !at_default_surplus() {
            return;
        }
        let yaml = cluster_pix_yaml();
        for key in [
            "num_ssa_parts: 512",
            "ssa_part_size: 64",
            &format!("additional_shares: {}", additional_shares()),
            "max_recovery_time: 900s",
            "fill_enabled: true",
            "fill_finish_fraction: 0.6",
        ] {
            assert!(yaml.contains(key), "missing `{key}` in:\n{yaml}");
        }
    }
}
