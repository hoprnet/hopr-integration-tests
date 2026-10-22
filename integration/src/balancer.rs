//! The entry's SURB balancer, sampled over time from this process's own registry.
//!
//! # Why this exists
//!
//! [`crate::session_metrics`] answers "how many packets reached the session" with counters read
//! once either side of a phase. The SURB self-congestion incident (2026-09-24, `gnosis_vpn-client`
//! v0.96.x on macOS) is about the *shape* of SURB production over time instead: the balancer's PID
//! sat at 0 most of the time and jumped to the full `max_surbs_per_sec` budget for a second or two
//! whenever its level estimate dipped below target, and when the return path was marked degraded it
//! stayed pinned at the budget with nothing coming back. Two readings around a phase average that
//! away, so this module samples the balancer's gauges and the session's SURB counters at a fixed
//! interval and keeps the series.
//!
//! Like `session_metrics`, the entry is `edgli` linked into the test binary, so its Prometheus
//! registry is this process's registry and is gathered directly.
//!
//! # What is read
//!
//! | family | kind | meaning |
//! | ------ | ---- | ------- |
//! | [`CONTROL_OUTPUT`] | gauge | the PID's command, SURB/s of keep-alive production (`output` in the debug log) |
//! | [`BUFFER_ESTIMATE`] | gauge | the entry's estimate of the exit's SURB store (`level`); forced to 0 while degraded |
//! | [`BUFFER_TARGET`] | gauge | the setpoint (`target`) |
//! | [`SURBS_PRODUCED`] | counter | SURBs handed to the sender, keep-alive and organic (`produced`) |
//! | [`SURBS_CONSUMED`] | counter | reply packets received, one SURB each (`consumed`) |
//! | [`LIFETIME_STATE`] | gauge | the session's lifecycle: Active=0, Closing=1, Closed=2 |
//!
//! The gauges exist only when `hopr-transport-session` is built with `telemetry` (edgli's
//! `telemetry` feature turns it on) and only once a balancer has ticked. Absence is reported as
//! `None`, never as zero — a zero `output` is a real and important reading here.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// PID control output, SURB/s.
pub const CONTROL_OUTPUT: &str = "hopr_surb_balancer_control_output";
/// Estimated SURBs held by the counterparty.
pub const BUFFER_ESTIMATE: &str = "hopr_surb_balancer_current_buffer_estimate";
/// The balancer's setpoint.
pub const BUFFER_TARGET: &str = "hopr_surb_balancer_current_buffer_target";
/// SURBs sent to the counterparty.
pub const SURBS_PRODUCED: &str = "hopr_session_surb_produced_total";
/// Reply packets received (each consumed one SURB at the counterparty).
pub const SURBS_CONSUMED: &str = "hopr_session_surb_consumed_total";
/// Session lifecycle state as the session manager sees it: Active=0, Closing=1, Closed=2.
///
/// What tells a session that is really gone from one whose handle was dropped while the manager
/// (and its balancer) carry on: the latter stays Active.
pub const LIFETIME_STATE: &str = "hopr_session_lifetime_state";

/// [`LIFETIME_STATE`]'s value for a live session.
pub const STATE_ACTIVE: f64 = 0.0;

/// One session's balancer state at one instant. Every field is `None` when its family is absent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BalancerReading {
    pub level: Option<f64>,
    pub target: Option<f64>,
    pub output: Option<f64>,
    pub produced: Option<u64>,
    pub consumed: Option<u64>,
    /// [`LIFETIME_STATE`], as reported by the session manager.
    pub state: Option<f64>,
}

/// Every session's reading at one instant, keyed by the opaque `session_id` label.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading(pub BTreeMap<String, BalancerReading>);

/// Parse the families this module reads out of Prometheus text exposition.
///
/// Lines for other families, comments and malformed values are skipped. A sample without a
/// `session_id` label is filed under the empty id rather than dropped, so a renamed label shows up
/// as a session nobody expected instead of as silence.
pub fn parse(text: &str) -> Reading {
    let mut out: BTreeMap<String, BalancerReading> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name_end = line.find(['{', ' ']).unwrap_or(line.len());
        let name = &line[..name_end];
        if ![
            CONTROL_OUTPUT,
            BUFFER_ESTIMATE,
            BUFFER_TARGET,
            SURBS_PRODUCED,
            SURBS_CONSUMED,
            LIFETIME_STATE,
        ]
        .contains(&name)
        {
            continue;
        }
        let session = session_label(&line[name_end..]).unwrap_or_default();
        let Some(value) = line
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        else {
            continue;
        };
        let entry = out.entry(session).or_default();
        match name {
            CONTROL_OUTPUT => entry.output = Some(value),
            BUFFER_ESTIMATE => entry.level = Some(value),
            BUFFER_TARGET => entry.target = Some(value),
            SURBS_PRODUCED => entry.produced = Some(value.max(0.0) as u64),
            SURBS_CONSUMED => entry.consumed = Some(value.max(0.0) as u64),
            LIFETIME_STATE => entry.state = Some(value),
            _ => {}
        }
    }
    Reading(out)
}

fn session_label(rest: &str) -> Option<String> {
    let labels = rest.strip_prefix('{')?.split('}').next()?;
    labels.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == "session_id").then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Read the balancer families out of this process's own registry.
pub fn read() -> Reading {
    match edgli::hopr_lib::collect_hopr_metrics() {
        Ok(text) => parse(&text),
        Err(e) => {
            tracing::warn!("could not gather in-process metrics: {e}");
            Reading::default()
        }
    }
}

/// One point of a [`Trace`]: time since the sampler started, and the session's reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Duration,
    pub reading: BalancerReading,
}

/// One session's balancer state over time.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub session: String,
    pub samples: Vec<Sample>,
}

impl Trace {
    /// Keep exactly the requested session, including when another live or leaked session
    /// produces more SURBs. Missing telemetry stays absent rather than selecting a substitute.
    pub fn from_readings(readings: &[(Duration, Reading)], session: &str) -> Self {
        let samples = readings
            .iter()
            .filter_map(|(at, r)| {
                r.0.get(session).map(|reading| Sample {
                    at: *at,
                    reading: *reading,
                })
            })
            .collect();
        Self {
            session: session.to_string(),
            samples,
        }
    }

    /// Whether the balancer gauges were seen at all (the build has telemetry and a balancer ran).
    pub fn observable(&self) -> bool {
        self.samples.iter().any(|s| s.reading.output.is_some())
    }

    /// The samples with `from <= at < to`.
    pub fn window(&self, from: Duration, to: Duration) -> Self {
        Self {
            session: self.session.clone(),
            samples: self
                .samples
                .iter()
                .filter(|s| s.at >= from && s.at < to)
                .copied()
                .collect(),
        }
    }

    /// The samples from `from` onwards.
    pub fn after(&self, from: Duration) -> Self {
        self.window(from, Duration::MAX)
    }

    fn span(&self) -> Option<(Sample, Sample)> {
        Some((*self.samples.first()?, *self.samples.last()?))
    }

    fn rate(&self, field: fn(&BalancerReading) -> Option<u64>) -> Option<f64> {
        let (first, last) = self.span()?;
        let dt = (last.at - first.at).as_secs_f64();
        if dt <= 0.0 {
            return None;
        }
        let delta = field(&last.reading)?.saturating_sub(field(&first.reading)?);
        Some(delta as f64 / dt)
    }

    /// Mean SURB production over the trace, SURB/s.
    pub fn mint_rate(&self) -> Option<f64> {
        self.rate(|r| r.produced)
    }

    /// Mean SURB consumption (reply packets in) over the trace, SURB/s.
    pub fn consume_rate(&self) -> Option<f64> {
        self.rate(|r| r.consumed)
    }

    /// SURBs produced between the first and last sample.
    pub fn produced_delta(&self) -> Option<u64> {
        let (first, last) = self.span()?;
        Some(
            last.reading
                .produced?
                .saturating_sub(first.reading.produced?),
        )
    }

    /// The highest production rate over any window at least `window` long, SURB/s.
    ///
    /// This is the burst a mean hides: the incident averaged a few hundred SURB/s over a minute
    /// while spending whole seconds at the full budget.
    pub fn peak_mint_rate(&self, window: Duration) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut j = 0;
        for i in 0..self.samples.len() {
            while j < self.samples.len() && self.samples[j].at < self.samples[i].at + window {
                j += 1;
            }
            let Some(end) = self.samples.get(j) else {
                break;
            };
            let start = &self.samples[i];
            let (Some(a), Some(b)) = (start.reading.produced, end.reading.produced) else {
                continue;
            };
            let rate = b.saturating_sub(a) as f64 / (end.at - start.at).as_secs_f64();
            best = Some(best.map_or(rate, |x: f64| x.max(rate)));
        }
        best
    }

    /// Share of samples whose control output was at least `threshold` SURB/s.
    pub fn share_output_at_least(&self, threshold: f64) -> Option<f64> {
        let outputs: Vec<f64> = self
            .samples
            .iter()
            .filter_map(|s| s.reading.output)
            .collect();
        if outputs.is_empty() {
            return None;
        }
        Some(outputs.iter().filter(|&&o| o >= threshold).count() as f64 / outputs.len() as f64)
    }

    /// Share of samples whose control output was exactly zero.
    pub fn share_output_zero(&self) -> Option<f64> {
        let outputs: Vec<f64> = self
            .samples
            .iter()
            .filter_map(|s| s.reading.output)
            .collect();
        if outputs.is_empty() {
            return None;
        }
        Some(outputs.iter().filter(|&&o| o <= 0.0).count() as f64 / outputs.len() as f64)
    }

    /// Share of samples with the estimated level more than `margin` below target and zero
    /// artificial production. Organic packets may still carry SURBs; this measures controller
    /// inactivity, not an empty exit buffer or proof of integral windup.
    pub fn share_starved_below_target(&self, margin: f64) -> Option<f64> {
        let judged: Vec<bool> = self
            .samples
            .iter()
            .filter_map(|s| {
                let r = &s.reading;
                Some(r.target? - r.level? > margin && r.output? <= 0.0)
            })
            .collect();
        if judged.is_empty() {
            return None;
        }
        Some(judged.iter().filter(|&&starved| starved).count() as f64 / judged.len() as f64)
    }

    /// The session's lifecycle state at the last sample, `None` when the family is absent.
    pub fn last_state(&self) -> Option<f64> {
        self.samples.iter().rev().find_map(|s| s.reading.state)
    }

    /// Samples in which the controller had overwritten its level with 0 and was producing at
    /// ≥95 % of `budget` — the signature of degraded mode (`sustain_on_return_path_loss`) in the
    /// current controller, which is not exported as a metric of its own.
    pub fn degraded_samples(&self, budget: f64) -> usize {
        self.samples
            .iter()
            .filter(|s| {
                s.reading.level == Some(0.0) && s.reading.output.is_some_and(|o| o >= 0.95 * budget)
            })
            .count()
    }

    /// Separate episodes — runs of consecutive samples — in which `hit` holds.
    fn episodes(&self, hit: impl Fn(&BalancerReading) -> bool) -> usize {
        let mut count = 0;
        let mut inside = false;
        for sample in &self.samples {
            let now = hit(&sample.reading);
            if now && !inside {
                count += 1;
            }
            inside = now;
        }
        count
    }

    /// Separate episodes with the control output at ≥95 % of `budget`. More than one during a
    /// single disturbance means production keeps returning to the budget — a loop, not a burst.
    pub fn episodes_at_budget(&self, budget: f64) -> usize {
        self.episodes(|r| r.output.is_some_and(|o| o >= 0.95 * budget))
    }

    /// Separate degraded-mode episodes, by the signature of [`Self::degraded_samples`].
    pub fn degraded_episodes(&self, budget: f64) -> usize {
        self.episodes(|r| r.level == Some(0.0) && r.output.is_some_and(|o| o >= 0.95 * budget))
    }

    /// Seconds spent with the control output at ≥95 % of `budget` (sample count × median spacing).
    pub fn seconds_at_budget(&self, budget: f64) -> f64 {
        let n = self
            .samples
            .iter()
            .filter(|s| s.reading.output.is_some_and(|o| o >= 0.95 * budget))
            .count();
        let mut gaps: Vec<f64> = self
            .samples
            .windows(2)
            .map(|w| (w[1].at - w[0].at).as_secs_f64())
            .collect();
        if gaps.is_empty() {
            return 0.0;
        }
        gaps.sort_by(f64::total_cmp);
        n as f64 * gaps[gaps.len() / 2]
    }

    /// One line for the run log.
    pub fn summary(&self) -> String {
        if !self.observable() {
            return "balancer gauges absent (no telemetry, or no balancer ran) — not zeroes".into();
        }
        let f = |v: Option<f64>| v.map_or("absent".to_string(), |v| format!("{v:.0}"));
        let pct = |v: Option<f64>| v.map_or("absent".to_string(), |v| format!("{:.0}%", v * 100.0));
        let max_output = self
            .samples
            .iter()
            .filter_map(|s| s.reading.output)
            .fold(0.0, f64::max);
        format!(
            "session {}: {} samples, mint {} SURB/s (peak 1s {}), consume {} SURB/s, output at 0 \
             {} / at ≥95% of observed max {} ({max_output:.0})",
            self.session,
            self.samples.len(),
            f(self.mint_rate()),
            f(self.peak_mint_rate(Duration::from_secs(1))),
            f(self.consume_rate()),
            pct(self.share_output_zero()),
            pct(self.share_output_at_least(0.95 * max_output.max(1.0))),
        )
    }

    /// Write the series as CSV (`t_s,level,target,output,produced,consumed`), empty cells for absent.
    pub fn write_csv(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut out = String::from("t_s,level,target,output,produced,consumed\n");
        let o = |v: Option<f64>| v.map(|v| format!("{v}")).unwrap_or_default();
        let u = |v: Option<u64>| v.map(|v| v.to_string()).unwrap_or_default();
        for s in &self.samples {
            let r = &s.reading;
            let _ = writeln!(
                out,
                "{:.3},{},{},{},{},{}",
                s.at.as_secs_f64(),
                o(r.level),
                o(r.target),
                o(r.output),
                u(r.produced),
                u(r.consumed),
            );
        }
        std::fs::write(path, out)
    }

    /// Write the CSV to `$SURB_BALANCER_CSV_DIR/<name>.csv` when that variable is set.
    pub fn maybe_write_csv(&self, name: &str) {
        let Some(dir) = std::env::var_os("SURB_BALANCER_CSV_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let path = dir.join(format!("{name}.csv"));
        match std::fs::create_dir_all(&dir).and_then(|_| self.write_csv(&path)) {
            Ok(()) => tracing::info!(path = %path.display(), "wrote balancer trace"),
            Err(e) => tracing::warn!(path = %path.display(), "could not write balancer trace: {e}"),
        }
    }
}

/// Samples [`read`] at a fixed interval on a background task until stopped.
pub struct Sampler {
    session: Option<String>,
    started: Instant,
    stop: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<Vec<(Duration, Reading)>>,
}

impl Sampler {
    /// Start sampling every `every` (the balancer itself ticks every 100 ms).
    pub fn start(every: Duration) -> Self {
        let started = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = tokio::spawn(async move {
            let mut readings = Vec::new();
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while !flag.load(Ordering::Relaxed) {
                tick.tick().await;
                readings.push((started.elapsed(), read()));
            }
            readings
        });
        Self {
            session: None,
            started,
            stop,
            handle,
        }
    }

    /// Bind this sampler to the session under test, before its handle is split or dropped.
    pub fn for_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Time since the sampler started, on the same clock as [`Sample::at`], for marking phases.
    pub fn now(&self) -> Duration {
        self.started.elapsed()
    }

    /// Stop sampling and return the data session's series.
    pub async fn stop(self) -> anyhow::Result<Trace> {
        self.stop.store(true, Ordering::Relaxed);
        let readings = self.handle.await?;
        Ok(Trace::from_readings(
            &readings,
            &self
                .session
                .ok_or_else(|| anyhow::anyhow!("sampler has no session id"))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"
# HELP hopr_surb_balancer_control_output Control output of the SURB balancer
# TYPE hopr_surb_balancer_control_output gauge
hopr_surb_balancer_control_output{session_id="a"} 3797
hopr_surb_balancer_control_output{session_id="b"} 0
hopr_surb_balancer_current_buffer_estimate{session_id="a"} 0
hopr_surb_balancer_current_buffer_target{session_id="a"} 9803
hopr_session_surb_produced_total{session_id="a"} 120000
hopr_session_surb_consumed_total{session_id="a"} 40000
hopr_session_lifetime_state{session_id="a"} 0
hopr_session_lifetime_state{session_id="b"} 2
hopr_session_frame_timeout_ms{session_id="a"} 800
hopr_packets_count{type="forwarded"} 5
"#;

    #[test]
    fn parse_should_read_each_family_per_session() {
        let r = parse(TEXT);
        let a = r.0["a"];
        assert_eq!(a.output, Some(3797.0));
        assert_eq!(
            a.level,
            Some(0.0),
            "a zero level is a reading, not an absence"
        );
        assert_eq!(a.target, Some(9803.0));
        assert_eq!(a.produced, Some(120_000));
        assert_eq!(a.consumed, Some(40_000));
        assert_eq!(a.state, Some(STATE_ACTIVE));
        assert_eq!(r.0["b"].state, Some(2.0));
        assert_eq!(r.0["b"].output, Some(0.0));
        assert_eq!(
            r.0["b"].produced, None,
            "an unregistered family must stay absent"
        );
        assert_eq!(r.0.len(), 2, "unrelated families must not create sessions");
    }

    fn reading(produced: u64, consumed: u64, output: f64) -> Reading {
        let mut m = BTreeMap::new();
        m.insert(
            "s".to_string(),
            BalancerReading {
                level: Some(0.0),
                target: Some(100.0),
                output: Some(output),
                produced: Some(produced),
                consumed: Some(consumed),
                state: Some(STATE_ACTIVE),
            },
        );
        Reading(m)
    }

    #[test]
    fn peak_should_find_a_burst_the_mean_hides() {
        // 10 s at 100 SURB/s, with one second at 5000 SURB/s in the middle.
        let mut readings = Vec::new();
        let mut produced = 0;
        for tenth in 0..=100u64 {
            let rate = if (40..50).contains(&tenth) { 5000 } else { 100 };
            readings.push((
                Duration::from_millis(tenth * 100),
                reading(produced, tenth * 10, if rate > 100 { 5000.0 } else { 0.0 }),
            ));
            produced += rate / 10;
        }
        let t = Trace::from_readings(&readings, "s");
        let mean = t.mint_rate().unwrap();
        let peak = t.peak_mint_rate(Duration::from_secs(1)).unwrap();
        assert!(mean < 700.0, "mean {mean}");
        assert!(peak > 4500.0, "peak {peak}");
        assert!((t.share_output_at_least(4750.0).unwrap() - 10.0 / 101.0).abs() < 1e-9);
        assert!((t.consume_rate().unwrap() - 100.0).abs() < 1e-9);
        assert!((t.seconds_at_budget(5000.0) - 1.0).abs() < 1e-6);
        assert_eq!(
            t.degraded_samples(5000.0),
            10,
            "level 0 at the budget reads as degraded"
        );
        assert_eq!(t.last_state(), Some(STATE_ACTIVE));
        assert_eq!(t.episodes_at_budget(5000.0), 1, "one contiguous burst");
        assert_eq!(t.degraded_episodes(5000.0), 1);
    }

    #[test]
    fn the_trace_should_follow_the_requested_session_even_when_another_leaks() {
        // "old" is a leaking session producing faster than the session under test.
        let at = |t: u64, old: u64, new: u64| {
            let mut m = BTreeMap::new();
            for (id, produced) in [("old", old), ("new", new)] {
                m.insert(
                    id.to_string(),
                    BalancerReading {
                        produced: Some(produced),
                        ..reading(0, 0, 0.0).0["s"]
                    },
                );
            }
            (Duration::from_secs(t), Reading(m))
        };
        let t = Trace::from_readings(
            &[at(0, 30_000, 0), at(1, 40_000, 500), at(2, 50_000, 900)],
            "new",
        );
        assert_eq!(t.session, "new");
        assert_eq!(t.produced_delta(), Some(900));
    }

    #[test]
    fn a_missing_requested_session_should_not_fall_back_to_another() {
        let trace = Trace::from_readings(&[(Duration::ZERO, reading(5000, 50, 100.0))], "missing");
        assert_eq!(trace.session, "missing");
        assert!(!trace.observable());
        assert_eq!(trace.produced_delta(), None);
    }

    #[test]
    fn episodes_should_count_separate_runs() {
        let outputs = [0.0, 5000.0, 5000.0, 0.0, 0.0, 5000.0, 0.0, 4000.0];
        let readings: Vec<_> = outputs
            .iter()
            .enumerate()
            .map(|(i, &o)| (Duration::from_secs(i as u64), reading(0, 0, o)))
            .collect();
        let t = Trace::from_readings(&readings, "s");
        assert_eq!(
            t.episodes_at_budget(5000.0),
            2,
            "4000 is below 95 % of the budget"
        );
        assert_eq!(t.episodes_at_budget(4000.0), 3);
        assert_eq!(Trace::default().episodes_at_budget(5000.0), 0);
    }

    #[test]
    fn starved_should_count_zero_output_below_target() {
        // Level 0 against target 100: below target in every sample, output 0 in 3 of 4.
        let readings: Vec<_> = (0..4u64)
            .map(|s| {
                let output = if s == 0 { 500.0 } else { 0.0 };
                (Duration::from_secs(s), reading(0, 0, output))
            })
            .collect();
        let t = Trace::from_readings(&readings, "s");
        assert!((t.share_starved_below_target(50.0).unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(
            t.share_starved_below_target(200.0),
            Some(0.0),
            "within the margin"
        );
        assert_eq!(Trace::default().share_starved_below_target(50.0), None);
    }

    #[test]
    fn window_should_slice_by_time() {
        let readings: Vec<_> = (0..10u64)
            .map(|s| (Duration::from_secs(s), reading(s * 10, 0, 0.0)))
            .collect();
        let t = Trace::from_readings(&readings, "s");
        let w = t.window(Duration::from_secs(2), Duration::from_secs(5));
        assert_eq!(w.samples.len(), 3);
        assert_eq!(w.produced_delta(), Some(20));
        assert_eq!(t.after(Duration::from_secs(8)).samples.len(), 2);
    }

    #[test]
    fn an_empty_trace_should_report_absence() {
        let t = Trace::default();
        assert!(!t.observable());
        assert_eq!(t.mint_rate(), None);
        assert_eq!(t.peak_mint_rate(Duration::from_secs(1)), None);
        assert!(t.summary().contains("absent"));
    }
}
