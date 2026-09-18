//! The Exit's `hopr_pix_*` supervisor and egress-gate aggregates, read from its `/metrics`.
//!
//! [`crate::pix`] reads the *settlement* family, `hopr_strategy_pix_*` — deposits made,
//! keys recovered, cycles swept. That answers "did the Exit get paid". It cannot answer why a
//! Session stopped making progress, because every cause reaches it as the same symptom: `sweeps`
//! failing to increment before the budget expires. A false gate stall, a starved SURB buffer, a
//! deposit that never mined and a busy host are one reading.
//!
//! This module reads the other family — the one hoprnet#8411, #8412 and #8413 added to the
//! supervision layer — which distinguishes them:
//!
//! | what | series |
//! | --- | --- |
//! | egress parked, and why | `hopr_pix_gate_blocks_total{reason}`, `hopr_pix_gate_block_seconds{reason}` |
//! | shares accepted, and whether they advanced recovery | `hopr_pix_shares_total{kind}` |
//! | packets served, and what paid for them | `hopr_pix_egress_packets_total{mode}` |
//! | how far each cycle got, once it finalized | `hopr_pix_cycle_{egress_packets,useful_share_fraction,accepted_share_fraction}{outcome}` |
//!
//! # Why a separate reader
//!
//! [`crate::pix::PixCounters`] is `u64`-and-delta-only, which is exactly right for a family of
//! monotonic counters and wrong for this one. These are three instrument kinds with three different
//! reading rules: counters subtract, live-set gauges do not (a delta of a gauge is meaningless), and
//! histograms are bucket vectors with float sums. Widening `PixCounters` would put all three behind
//! one `get() -> Option<u64>` and hand every caller a number whose meaning depends on which series
//! it came from. hoprnet kept the same two families apart in the same way and for the same reason
//! (`transport/session/src/telemetry/mod.rs`).
//!
//! # Absent is not zero
//!
//! Carried through from [`crate::pix`], because it matters more here. A `MultiCounter` label
//! materialises when it is first incremented, so a run in which the gate never parked has **no**
//! `hopr_pix_gate_blocks_total{reason="share_lag"}` series at all. `None` from
//! [`ExitTelemetry::gate_blocks`] is therefore the *good* outcome, and
//! [`ExitTelemetry::observable`] is what separates it from "this Exit exports no PIX aggregates,
//! so nothing was measuring".
//!
//! # The exposition format
//!
//! Derived from `opentelemetry-prometheus-text-exporter`, which is what `hoprd` serves `/metrics`
//! through, rather than assumed. Four properties the parser below depends on:
//!
//! * A histogram named `N` renders as `N_count{A}`, `N_sum{A}`, one cumulative `N_bucket{A,le=B}`
//!   per bound, and a final `N_bucket{A,le="+Inf"}` equal to `N_count`.
//! * `le` is emitted **after** the attributes, and the attributes are not sorted. So a series is
//!   keyed on a *normalised* label set (see [`series_key`]) rather than on the raw label substring,
//!   which could otherwise vary between two scrapes of the same series.
//! * `le` is `f64::to_string`, so the 1.0 boundary arrives as `le="1"` and the comparison has to be
//!   numeric. [`Histogram::above`] is the accessor that does it.
//! * Buckets are **cumulative**, which is what makes both [`Histogram::above`] and the bucket-wise
//!   subtraction in [`ExitTelemetry::delta`] correct.
//!
//! `_total` is appended only when a name does not already end in it, and a unit suffix only when the
//! instrument declares a unit — `hopr-types`' wrappers never do. So every name renders as declared.

use std::{collections::BTreeMap, time::Duration};

use crate::{cluster::NodeInfo, pix::label_value};

/// The family this module reads. Names are matched on the prefix *and* re-split on the brace, so
/// nothing from the `hopr_strategy_pix_*` family can leak in.
const PREFIX: &str = "hopr_pix_";

// ── Series names ─────────────────────────────────────────────────────────────

/// Shares the supervisor validated and newly accepted, by whether they advanced reconstruction.
///
/// `kind` is `PixShareKind`, rendered snake_case: `useful` or `surplus`. The pair is the whole
/// surplus-run question — a conforming Entry's surplus advances `surplus` and leaves `useful`
/// untouched, which is the run the egress gate must not mistake for silence.
const SHARES: &str = "hopr_pix_shares_total";

/// Egress block *episodes* — counted once when the gate parks, not once per refused packet.
///
/// `reason` is `GateBlockReason`: `share_lag` or `predeposit_exhausted`. The first is the one this
/// repo's scenarios assert stays at zero.
const GATE_BLOCKS: &str = "hopr_pix_gate_blocks_total";

/// How long the gate stayed parked, per episode, by the same `reason`.
const GATE_BLOCK_SECONDS: &str = "hopr_pix_gate_block_seconds";

/// Data packets the egress gate admitted, by what paid for them: `predeposit` or `funded`.
const EGRESS_PACKETS: &str = "hopr_pix_egress_packets_total";

/// Cycle lifecycle transitions, by `event`: `requested`, `committed`, `funded`, `recovered`,
/// `failed`, `retired`.
const CYCLES: &str = "hopr_pix_cycles_total";

/// Packets served while one cycle held the accounting front, observed once at finalization.
const CYCLE_EGRESS: &str = "hopr_pix_cycle_egress_packets";

/// Useful shares over target for one cycle, observed once at finalization. At most 1.0.
const USEFUL_FRACTION: &str = "hopr_pix_cycle_useful_share_fraction";

/// *Accepted* shares over the useful-share target, observed once at finalization.
///
/// Exceeds one exactly when the Entry sent its negotiated surplus and the Exit took it: a complete
/// cycle lands at `(threshold + surplus) / threshold`. Reading a recovered cycle above the 1.0
/// bucket is the single-sample proof that the surplus was served, needing no trace at all.
const ACCEPTED_FRACTION: &str = "hopr_pix_cycle_accepted_share_fraction";

// ── Histogram ────────────────────────────────────────────────────────────────

/// One histogram series: its cumulative buckets, its sum and its observation count.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Histogram {
    /// `(le, cumulative count)`, ascending. `+Inf` is kept, so the last entry equals `count`.
    buckets: Vec<(f64, u64)>,
    sum: f64,
    count: u64,
}

impl Histogram {
    /// Observations recorded, from `_count`.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Sum of the observed values, from `_sum`.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Mean observation, or `None` when nothing was observed.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// Observations **strictly above** `le`.
    ///
    /// `count` minus the cumulative count at the bucket whose bound is `le`. The bound is matched
    /// numerically rather than by string, because the exporter renders it with `f64::to_string` and
    /// the 1.0 boundary therefore arrives as `le="1"` — a string comparison against `"1.0"` would
    /// silently find nothing and report every observation as above the bound.
    ///
    /// `None` when no bucket has that bound: the caller asked about a boundary this histogram was
    /// not configured with, which is a mistake in the caller rather than a fact about the Exit.
    pub fn above(&self, le: f64) -> Option<u64> {
        self.buckets
            .iter()
            .find(|(bound, _)| (bound - le).abs() < f64::EPSILON)
            .map(|(_, cumulative)| self.count.saturating_sub(*cumulative))
    }

    /// Counts accumulated between `self` (earlier) and `later`, bucket by bucket.
    ///
    /// Valid because the buckets are cumulative counters: the difference of two cumulative vectors
    /// is itself a cumulative vector. Bounds present only in `later` are taken whole; a bound that
    /// vanished is dropped, matching the saturating spirit of [`ExitTelemetry::delta`].
    fn delta(&self, later: &Self) -> Self {
        Self {
            buckets: later
                .buckets
                .iter()
                .map(|(bound, cumulative)| {
                    let before = self
                        .buckets
                        .iter()
                        .find(|(b, _)| (b - bound).abs() < f64::EPSILON)
                        .map(|(_, c)| *c)
                        .unwrap_or_default();
                    (*bound, cumulative.saturating_sub(before))
                })
                .collect(),
            sum: later.sum - self.sum,
            count: later.count.saturating_sub(self.count),
        }
    }
}

// ── Reading ──────────────────────────────────────────────────────────────────

/// One reading of the `hopr_pix_*` family, split by instrument kind.
///
/// The three maps are separate because their reading rules are: see the module header. Keys are
/// [`series_key`]'s normalised `family{label="value",…}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExitTelemetry {
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
    hists: BTreeMap<String, Histogram>,
}

impl ExitTelemetry {
    /// A counter series by its full label set; `&[]` for the unlabelled ones.
    pub fn counter_series(&self, family: &str, labels: &[(&str, &str)]) -> Option<u64> {
        self.counters.get(&series_key(family, labels)).copied()
    }

    /// A counter series carrying one label, which is every labelled counter in this family.
    pub fn counter(&self, family: &str, label: &str, value: &str) -> Option<u64> {
        self.counter_series(family, &[(label, value)])
    }

    /// An unlabelled gauge's current value.
    pub fn gauge(&self, family: &str) -> Option<f64> {
        self.gauges.get(&series_key(family, &[])).copied()
    }

    /// A histogram series.
    pub fn histogram(&self, family: &str, label: &str, value: &str) -> Option<&Histogram> {
        self.hists.get(&series_key(family, &[(label, value)]))
    }

    /// Shares newly accepted of one `kind` — `"useful"` or `"surplus"`.
    pub fn shares(&self, kind: &str) -> Option<u64> {
        self.counter(SHARES, "kind", kind)
    }

    /// Egress block episodes for one `reason` — `"share_lag"` or `"predeposit_exhausted"`.
    ///
    /// `None` means the gate never parked for that reason, which is the healthy reading. See the
    /// module header on absent-versus-zero.
    pub fn gate_blocks(&self, reason: &str) -> Option<u64> {
        self.counter(GATE_BLOCKS, "reason", reason)
    }

    /// How long the gate stayed parked, per episode, for one `reason`.
    pub fn gate_block_seconds(&self, reason: &str) -> Option<&Histogram> {
        self.histogram(GATE_BLOCK_SECONDS, "reason", reason)
    }

    /// Packets admitted in one gate `mode` — `"predeposit"` or `"funded"`.
    pub fn egress(&self, mode: &str) -> Option<u64> {
        self.counter(EGRESS_PACKETS, "mode", mode)
    }

    /// Cycle transitions of one `event`.
    pub fn cycles(&self, event: &str) -> Option<u64> {
        self.counter(CYCLES, "event", event)
    }

    /// Per-cycle egress distribution for one `outcome` — `"recovered"` or `"failed"`.
    pub fn cycle_egress(&self, outcome: &str) -> Option<&Histogram> {
        self.histogram(CYCLE_EGRESS, "outcome", outcome)
    }

    /// Per-cycle useful-share coverage for one `outcome`.
    pub fn useful_fraction(&self, outcome: &str) -> Option<&Histogram> {
        self.histogram(USEFUL_FRACTION, "outcome", outcome)
    }

    /// Per-cycle *accepted*-share coverage for one `outcome`; above 1.0 means surplus was taken.
    pub fn accepted_fraction(&self, outcome: &str) -> Option<&Histogram> {
        self.histogram(ACCEPTED_FRACTION, "outcome", outcome)
    }

    /// Whether any `hopr_pix_*` series exists at all.
    ///
    /// `false` means the Exit exports no PIX aggregates — built without
    /// `hopr-transport-session/telemetry`, or a rev predating hoprnet#8411 — so every accessor
    /// above returns `None` for a reason that has nothing to do with the Session under test.
    pub fn observable(&self) -> bool {
        !(self.counters.is_empty() && self.gauges.is_empty() && self.hists.is_empty())
    }

    /// Counters and histograms accumulated between `self` (earlier) and `later`; gauges taken from
    /// `later` as they stand.
    ///
    /// Gauges are deliberately *not* differenced. They are a live-set census — Sessions active,
    /// cycles by phase, bytes reserved — and the difference of two censuses is not a quantity: a
    /// Session opening and another closing between readings nets to zero and reads as "nothing
    /// happened". The absolute value at the later reading is the only thing that means anything.
    pub fn delta(&self, later: &Self) -> Self {
        Self {
            counters: later
                .counters
                .iter()
                .map(|(name, &v)| {
                    (
                        name.clone(),
                        v.saturating_sub(self.counters.get(name).copied().unwrap_or_default()),
                    )
                })
                .collect(),
            gauges: later.gauges.clone(),
            hists: later
                .hists
                .iter()
                .map(|(name, h)| {
                    let before = self.hists.get(name).cloned().unwrap_or_default();
                    (name.clone(), before.delta(h))
                })
                .collect(),
        }
    }

    /// One line for the run log, naming what the gate and the shares did.
    ///
    /// Unregistered series print as `absent` rather than `0`, so a log line can never read as "the
    /// gate never blocked" when the truth is that nothing was measuring.
    pub fn summary(&self) -> String {
        if !self.observable() {
            return "PIX Exit aggregates unavailable (no hopr_pix_* series) — these are not zeroes"
                .to_string();
        }
        let show = |label: &str, value: Option<u64>| match value {
            Some(v) => format!("{label}={v}"),
            None => format!("{label}=absent"),
        };
        [
            show("useful", self.shares("useful")),
            show("surplus", self.shares("surplus")),
            show("blocks/share_lag", self.gate_blocks("share_lag")),
            show(
                "blocks/predeposit",
                self.gate_blocks("predeposit_exhausted"),
            ),
            show("egress/funded", self.egress("funded")),
            show("egress/predeposit", self.egress("predeposit")),
            show("cycles/recovered", self.cycles("recovered")),
            show("cycles/requested", self.cycles("requested")),
        ]
        .join(" ")
    }
}

/// The Exit's PIX aggregates, scraped from its `/metrics`.
///
/// The counterpart to [`crate::pix::sample_exit`], which parses the settlement family out of the
/// same body. `hoprd` strips only `hopr_session_*` from that endpoint, so this family survives it.
pub async fn sample(node: &NodeInfo) -> anyhow::Result<ExitTelemetry> {
    Ok(parse(&crate::cluster::scrape_metrics(node).await?))
}

// ── Sampling a whole scenario ────────────────────────────────────────────────

/// Seconds between samples, before `HOPRD_PIX_TRACE_POLL` overrides it.
///
/// Sized against the run it exists to see. A surplus-only run is
/// `SHARE_EMISSION_WINDOW x surplus` shares — 4096 at this repo's geometry — and at the ~232
/// packets/s a 1-hop cluster achieves that is ~18 s, so 2 s puts roughly eight samples inside it.
/// A cycle finished by Exit fill runs slower still (~120 packets/s, ~34 s), so the tight case is
/// the saturated one.
const DEFAULT_TRACE_POLL: Duration = Duration::from_secs(2);

/// Useful shares a step may carry and still count as surplus-only.
///
/// **Zero — deliberately strict.** In a window's surplus section every share the Entry sends is
/// surplus by construction, so the honest first measurement is to demand exactly that and see
/// whether it holds. It may not: a useful share lost earlier in the window leaves a polynomial one
/// short, and the first surplus share that fills the gap is counted useful instead. At the 99.9-100 %
/// arrival these scenarios measure that is a handful per 4096.
///
/// If the first full pass shows runs breaking up, raise this to a documented *ratio* of the step's
/// surplus — `useful * 32 <= surplus`, i.e. 3 % — rather than lowering the 2048 bar the assertion
/// exists to clear. Record in `docs/pix-traffic-shapes.md` which form the measurement justified.
const SURPLUS_RUN_USEFUL_TOLERANCE: u64 = 0;

fn trace_poll() -> Duration {
    std::env::var("HOPRD_PIX_TRACE_POLL")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TRACE_POLL)
}

/// The few series a trace follows over time, rather than a whole [`ExitTelemetry`] per tick.
///
/// A scenario is ten to twenty minutes at a 2 s cadence, so this is hundreds of entries; keeping
/// whole readings would hold every histogram bucket of every series for no reason. Everything the
/// per-cycle assertions need is a before/after pair, not a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    at: Duration,
    useful: u64,
    surplus: u64,
    share_lag_blocks: u64,
    egress_funded: u64,
    egress_predeposit: u64,
}

impl Sample {
    /// `None` collapses to zero here, and correctly: a failed scrape is never pushed as a sample,
    /// so an absent series at this point means it has not been incremented yet.
    fn of(at: Duration, t: &ExitTelemetry) -> Self {
        Self {
            at,
            useful: t.shares("useful").unwrap_or(0),
            surplus: t.shares("surplus").unwrap_or(0),
            share_lag_blocks: t.gate_blocks("share_lag").unwrap_or(0),
            egress_funded: t.egress("funded").unwrap_or(0),
            egress_predeposit: t.egress("predeposit").unwrap_or(0),
        }
    }
}

/// What the Exit's share counters did over a scenario, sampled.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    samples: Vec<Sample>,
}

impl Trace {
    /// Samples taken. Zero means every scrape failed, which is not the same as a quiet Exit.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The longest **contiguous** run of accepted surplus shares during which no useful share
    /// arrived, in shares.
    ///
    /// This is the quantity that distinguishes "the cycle accepted 4096 surplus shares somewhere"
    /// from "it accepted them as the uninterrupted run the emission window actually produces" —
    /// and the run is what the egress gate has to serve through without mistaking it for silence.
    /// Upstream emits one per window (`protocols/pix/src/generator.rs`: a window emits its entire
    /// surplus before the next starts), so a cycle of 1024 polynomials contains four.
    ///
    /// Strictness is [`SURPLUS_RUN_USEFUL_TOLERANCE`]'s; see there before relaxing it.
    pub fn longest_surplus_only_run(&self) -> u64 {
        let mut longest = 0;
        let mut current = 0;
        for pair in self.samples.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            let useful = after.useful.saturating_sub(before.useful);
            let surplus = after.surplus.saturating_sub(before.surplus);
            if useful > SURPLUS_RUN_USEFUL_TOLERANCE {
                current = 0;
                continue;
            }
            current += surplus;
            longest = longest.max(current);
        }
        longest
    }

    /// Whether the gate's `share_lag` counter moved at any point, which a before/after pair would
    /// miss if it had also resumed.
    pub fn saw_share_lag_block(&self) -> bool {
        self.samples
            .windows(2)
            .any(|p| p[1].share_lag_blocks > p[0].share_lag_blocks)
    }

    /// One line for the run log.
    pub fn summary(&self) -> String {
        let Some((first, last)) = self.samples.first().zip(self.samples.last()) else {
            return "no PIX telemetry samples were taken".to_string();
        };
        format!(
            "samples={} over {}s useful=+{} surplus=+{} longest_surplus_run={} \
             egress=+{}/funded +{}/predeposit share_lag_episodes=+{}",
            self.samples.len(),
            last.at.saturating_sub(first.at).as_secs(),
            last.useful.saturating_sub(first.useful),
            last.surplus.saturating_sub(first.surplus),
            self.longest_surplus_only_run(),
            last.egress_funded.saturating_sub(first.egress_funded),
            last.egress_predeposit
                .saturating_sub(first.egress_predeposit),
            last.share_lag_blocks.saturating_sub(first.share_lag_blocks),
        )
    }
}

/// A background task sampling one Exit's PIX aggregates until it is told to stop.
///
/// Spawned rather than run under a `join!` with the traffic, because the run it has to see spans
/// both the traffic phase and the wait for sweeps that follows it — and folding it into either
/// would mean restructuring scenarios that work.
///
/// It is deliberately cheap. `cluster::scrape_metrics` goes through one pooled client, a failed
/// scrape logs and is skipped rather than ending the trace, and nothing here can fail a scenario:
/// the shape this matters most to is the one already measured dropping its p2p connections when the
/// host is busy, and a harness that watches the Exit must not become what the Exit has to survive.
pub struct Sampler {
    handle: tokio::task::JoinHandle<Trace>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Sampler {
    /// Begin sampling `exit`.
    pub fn start(exit: &NodeInfo) -> Self {
        let exit = exit.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let poll = trace_poll();
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut samples = Vec::new();
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                match sample(&exit).await {
                    Ok(reading) => samples.push(Sample::of(started.elapsed(), &reading)),
                    Err(error) => {
                        tracing::warn!(%error, "PIX telemetry scrape failed; the trace skips this tick")
                    }
                }
                tokio::time::sleep(poll).await;
            }
            Trace { samples }
        });
        Self { handle, stop }
    }

    /// Stop sampling and collect the trace.
    ///
    /// Returns an empty trace rather than erroring if the task was cancelled or panicked: a trace
    /// is diagnostic, and the assertion that reads it is what names the consequence.
    pub async fn finish(self) -> Trace {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        match self.handle.await {
            Ok(trace) => trace,
            Err(error) => {
                tracing::warn!(%error, "the PIX telemetry sampler did not finish cleanly");
                Trace::default()
            }
        }
    }
}

/// The canonical key for one series: `family{label="value",…}`, labels sorted by name.
///
/// Sorted because the exporter emits attributes in the order the SDK hands them over, with no
/// ordering guarantee, and `le` appended after them. Keying on the raw label substring would let
/// one series produce two keys across two scrapes; normalising costs an allocation per series and
/// removes the possibility.
fn series_key(family: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return family.to_string();
    }
    let mut pairs: Vec<String> = labels
        .iter()
        .map(|(name, value)| format!("{name}=\"{value}\""))
        .collect();
    pairs.sort();
    format!("{family}{{{}}}", pairs.join(","))
}

/// Which instrument kind a rendered name belongs to, and the family it belongs to.
enum Kind<'a> {
    /// A histogram's `_bucket`, carrying the `le` bound.
    Bucket(&'a str),
    /// A histogram's `_sum`.
    Sum(&'a str),
    /// A histogram's `_count`.
    Count(&'a str),
    /// A monotonic counter — every one in this family is named `*_total`.
    Counter(&'a str),
    /// Anything else: a live-set gauge.
    Gauge(&'a str),
}

/// Classify a rendered series name.
///
/// Suffix order matters: `hopr_pix_cycle_bytes_reserved_total` is a counter and
/// `hopr_pix_cycle_egress_packets_count` is a histogram's count, so the histogram suffixes are
/// tested first and `_total` only afterwards.
fn classify(name: &str) -> Kind<'_> {
    if let Some(family) = name.strip_suffix("_bucket") {
        Kind::Bucket(family)
    } else if let Some(family) = name.strip_suffix("_sum") {
        Kind::Sum(family)
    } else if let Some(family) = name.strip_suffix("_count") {
        Kind::Count(family)
    } else if name.ends_with("_total") {
        Kind::Counter(name)
    } else {
        Kind::Gauge(name)
    }
}

/// Parse `le`, including the `+Inf` terminator the exporter always emits.
fn parse_le(raw: &str) -> Option<f64> {
    match raw {
        "+Inf" | "+inf" => Some(f64::INFINITY),
        other => other.parse().ok(),
    }
}

/// Read every `hopr_pix_*` series out of a Prometheus text exposition.
fn parse(body: &str) -> ExitTelemetry {
    let mut out = ExitTelemetry::default();

    for line in body.lines().filter(|l| !l.starts_with('#')) {
        // The name ends at the label brace or the value separator. Splitting on the prefix alone
        // would fold `_total_bytes` into `_total`, which is the trap `crate::pix::parse` documents.
        let Some(name) = line.split(['{', ' ']).next() else {
            continue;
        };
        if !name.starts_with(PREFIX) {
            continue;
        }
        let labels = line
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(labels, _)| labels)
            .unwrap_or_default();
        let Some(value) = line
            .rsplit_once(' ')
            .and_then(|(_, v)| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
        else {
            continue;
        };

        match classify(name) {
            Kind::Bucket(family) => {
                let Some(le) = label_value(labels, "le").and_then(parse_le) else {
                    continue;
                };
                let key = series_key(family, &without_le(labels));
                let hist = out.hists.entry(key).or_default();
                hist.buckets.push((le, value.max(0.0) as u64));
                // Ascending, so `above` can stop at the first matching bound and a reader of the
                // vector sees the cumulative shape the exporter intended.
                hist.buckets.sort_by(|(a, _), (b, _)| {
                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            Kind::Sum(family) => {
                out.hists
                    .entry(series_key(family, &without_le(labels)))
                    .or_default()
                    .sum = value;
            }
            Kind::Count(family) => {
                out.hists
                    .entry(series_key(family, &without_le(labels)))
                    .or_default()
                    .count = value.max(0.0) as u64;
            }
            Kind::Counter(family) => {
                // Registered-but-unset series still create the entry, which is the whole
                // absent-versus-zero distinction, so insert before adding the value.
                let entry = out
                    .counters
                    .entry(series_key(family, &without_le(labels)))
                    .or_default();
                *entry += value.max(0.0) as u64;
            }
            Kind::Gauge(family) => {
                out.gauges
                    .insert(series_key(family, &without_le(labels)), value);
            }
        }
    }

    out
}

/// Every label except `le`, as `series_key` wants them.
///
/// `le` is dropped because it distinguishes buckets *within* one histogram series rather than one
/// series from another: keeping it would make each bound its own series and no histogram would ever
/// be assembled.
fn without_le(labels: &str) -> Vec<(&str, &str)> {
    labels
        .split(',')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            (key != "le").then_some((key, value.trim().trim_matches('"')))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exposition `hoprd` actually serves, in the exporter's layout: `_count` and `_sum` ahead
    /// of the cumulative buckets, `le` last in the label set, `+Inf` closing each histogram.
    ///
    /// The accepted-share fractions are a complete cycle at this repo's shape geometry —
    /// `(64 + 16) / 64 = 1.25` — so the 1.0 bucket holds nothing and the 1.25 bucket holds the
    /// cycle. That is the arithmetic `the_accepted_fraction_shows_surplus_was_taken` asserts on.
    const EXPOSITION: &str = "\
# HELP hopr_pix_shares_total Supervisor-validated SSA shares
# TYPE hopr_pix_shares_total counter
hopr_pix_shares_total{kind=\"useful\"} 16384
hopr_pix_shares_total{kind=\"surplus\"} 4096
hopr_pix_egress_packets_total{mode=\"funded\"} 20480
hopr_pix_egress_packets_total{mode=\"predeposit\"} 512
hopr_pix_cycles_total{event=\"requested\"} 2
hopr_pix_cycles_total{event=\"recovered\"} 1
hopr_pix_live_cycle_bytes 85032960
hopr_pix_cycles_active{phase=\"recovering\"} 1
hopr_pix_cycle_accepted_share_fraction_count{outcome=\"recovered\"} 1
hopr_pix_cycle_accepted_share_fraction_sum{outcome=\"recovered\"} 1.25
hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"0.9\"} 0
hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1\"} 0
hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1.25\"} 1
hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1.5\"} 1
hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"+Inf\"} 1
";

    #[test]
    fn the_share_kinds_should_stay_apart() {
        let t = parse(EXPOSITION);
        assert_eq!(Some(16384), t.shares("useful"));
        assert_eq!(Some(4096), t.shares("surplus"));
    }

    /// The reading the scenarios are built on: a gate that never parked has no series at all, and
    /// that has to arrive as `None` rather than as a zero nobody measured.
    #[test]
    fn a_gate_that_never_blocked_should_be_absent_not_zero() {
        let t = parse(EXPOSITION);
        assert_eq!(None, t.gate_blocks("share_lag"));
        assert!(
            t.observable(),
            "the family is present, so `absent` here means the episode never happened"
        );
        assert!(!parse("").observable(), "an empty body measured nothing");
    }

    /// `le` is `f64::to_string`, so the 1.0 boundary is rendered `le="1"`. A string comparison
    /// against `"1.0"` would find no bucket and this is what catches it.
    #[test]
    fn the_accepted_fraction_shows_surplus_was_taken() {
        let t = parse(EXPOSITION);
        let h = t
            .accepted_fraction("recovered")
            .expect("the recovered cycle's accepted fraction");
        assert_eq!(1, h.count());
        assert_eq!(
            Some(1),
            h.above(1.0),
            "the cycle accepted more than its useful target"
        );
        assert_eq!(
            Some(0),
            h.above(1.25),
            "and exactly its negotiated surplus, no more"
        );
    }

    /// A bound the histogram was never configured with is a caller mistake, not a zero.
    #[test]
    fn an_unconfigured_bound_should_not_read_as_zero() {
        let t = parse(EXPOSITION);
        let h = t.accepted_fraction("recovered").expect("histogram");
        assert_eq!(None, h.above(0.42));
    }

    /// Attributes are emitted unsorted and `le` comes last, so the same series has to key the same
    /// way however the exporter happened to order it.
    #[test]
    fn label_order_should_not_split_a_series() {
        let a = parse("hopr_pix_gate_block_seconds_bucket{reason=\"share_lag\",le=\"0.5\"} 3\n");
        let b = parse("hopr_pix_gate_block_seconds_bucket{le=\"0.5\",reason=\"share_lag\"} 3\n");
        assert_eq!(a, b);
        assert!(a.gate_block_seconds("share_lag").is_some());
    }

    /// Histogram suffixes are stripped before `_total` is considered, or a counter whose name ends
    /// in `_total` and a histogram's `_count` would land in each other's maps.
    #[test]
    fn a_counter_named_total_should_not_be_read_as_a_histogram() {
        let t = parse("hopr_pix_cycle_bytes_reserved_total 4096\n");
        assert_eq!(
            Some(4096),
            t.counter_series("hopr_pix_cycle_bytes_reserved_total", &[])
        );
        assert!(
            t.hists.is_empty(),
            "no histogram should have been assembled"
        );
    }

    /// Counters and histograms accumulate; gauges are a census and must be taken as they stand.
    #[test]
    fn a_delta_should_difference_counters_and_keep_gauges_absolute() {
        let before = parse(EXPOSITION);
        // Every bound, as the exporter always emits them: `above` reports `None` for a bound the
        // histogram does not carry, so a fixture missing `le="1"` would be testing that instead.
        let later = parse(
            "hopr_pix_shares_total{kind=\"surplus\"} 8192\n\
             hopr_pix_live_cycle_bytes 42\n\
             hopr_pix_cycle_accepted_share_fraction_count{outcome=\"recovered\"} 3\n\
             hopr_pix_cycle_accepted_share_fraction_sum{outcome=\"recovered\"} 3.75\n\
             hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"0.9\"} 0\n\
             hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1\"} 0\n\
             hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1.25\"} 3\n\
             hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"1.5\"} 3\n\
             hopr_pix_cycle_accepted_share_fraction_bucket{outcome=\"recovered\",le=\"+Inf\"} 3\n",
        );
        let d = before.delta(&later);
        assert_eq!(Some(4096), d.shares("surplus"), "4096 more accepted since");
        assert_eq!(
            Some(42.0),
            d.gauge("hopr_pix_live_cycle_bytes"),
            "a gauge is the later census, not a difference of two"
        );
        let h = d.accepted_fraction("recovered").expect("histogram");
        assert_eq!(2, h.count(), "two cycles finalized in the window");
        assert_eq!(Some(2), h.above(1.0));
    }

    /// A family that disappeared between readings must not underflow into a vast number.
    #[test]
    fn a_delta_should_saturate_rather_than_wrap() {
        let before = parse("hopr_pix_shares_total{kind=\"surplus\"} 100\n");
        let later = parse("hopr_pix_shares_total{kind=\"surplus\"} 3\n");
        assert_eq!(Some(0), before.delta(&later).shares("surplus"));
    }

    /// The other PIX family shares the endpoint and must not leak into this one.
    #[test]
    fn the_settlement_family_should_be_ignored() {
        let t =
            parse("hopr_strategy_pix_sweeps_total 5\nhopr_pix_shares_total{kind=\"useful\"} 7\n");
        assert_eq!(Some(7), t.shares("useful"));
        assert_eq!(1, t.counters.len(), "only the hopr_pix_* counter was taken");
    }

    #[test]
    fn a_summary_should_name_absent_series_rather_than_printing_zero() {
        let s = parse(EXPOSITION).summary();
        assert!(s.contains("surplus=4096"), "{s}");
        assert!(s.contains("blocks/share_lag=absent"), "{s}");
        assert!(
            parse("").summary().contains("not zeroes"),
            "an unmeasured Exit must say so"
        );
    }

    // ── The run detector ─────────────────────────────────────────────────────

    /// A trace from `(useful, surplus)` cumulative readings, one per tick.
    fn trace_of(readings: &[(u64, u64)]) -> Trace {
        Trace {
            samples: readings
                .iter()
                .enumerate()
                .map(|(i, &(useful, surplus))| Sample {
                    at: Duration::from_secs(i as u64 * 2),
                    useful,
                    surplus,
                    share_lag_blocks: 0,
                    egress_funded: 0,
                    egress_predeposit: 0,
                })
                .collect(),
        }
    }

    /// The shape a window's surplus section actually produces: useful flat, surplus climbing.
    #[test]
    fn a_surplus_only_run_should_be_measured_across_its_steps() {
        let t = trace_of(&[
            (16_384, 0),
            (16_384, 1_100),
            (16_384, 2_300),
            (16_384, 3_400),
            (16_384, 4_096),
        ]);
        assert_eq!(4_096, t.longest_surplus_only_run());
    }

    /// A useful share ends the run, and the longest of the runs either side is what counts.
    ///
    /// This is the assertion's real defence: 4096 surplus shares scattered across a cycle in
    /// 500-share fragments must not read as the contiguous run the gate had to serve through.
    #[test]
    fn a_useful_share_should_end_the_run() {
        let t = trace_of(&[
            (16_000, 0),
            (16_000, 500),   // run of 500
            (16_100, 700),   // useful moved -- run ends
            (16_100, 2_500), // new run
            (16_100, 4_600), // ...still going
            (16_384, 4_700), // useful again
        ]);
        assert_eq!(
            4_600 - 700,
            t.longest_surplus_only_run(),
            "the second run is the longer one"
        );
    }

    /// A cycle carried entirely by useful shares has no surplus run at all, which is what makes
    /// the >= 2048 assertion meaningful rather than automatic.
    #[test]
    fn a_trace_with_no_surplus_should_measure_no_run() {
        let t = trace_of(&[(0, 0), (4_000, 0), (8_000, 0)]);
        assert_eq!(0, t.longest_surplus_only_run());
    }

    /// An Exit nobody could scrape is not an Exit that stayed quiet.
    #[test]
    fn an_empty_trace_should_be_visibly_empty() {
        let t = Trace::default();
        assert!(t.is_empty());
        assert_eq!(0, t.longest_surplus_only_run());
        assert!(
            t.summary().contains("no PIX telemetry samples"),
            "{}",
            t.summary()
        );
    }

    /// A gate that parked and resumed inside the window leaves both endpoints equal only if the
    /// counter were reset, which it is not — but reading the trace is what catches an episode that
    /// a coarser before/after pair would have to infer.
    #[test]
    fn a_block_episode_should_be_visible_in_the_trace() {
        let mut t = trace_of(&[(0, 0), (0, 2_048), (0, 4_096)]);
        t.samples[1].share_lag_blocks = 1;
        t.samples[2].share_lag_blocks = 1;
        assert!(t.saw_share_lag_block());
        assert!(!trace_of(&[(0, 0), (0, 2_048)]).saw_share_lag_block());
    }
}
