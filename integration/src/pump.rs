//! Reusable data-pump building block for scenarios.

use std::time::Duration;

use edgli::hopr_lib::exports::transport::{HoprSession, SESSION_MTU};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Received throughput in KiB/s over `elapsed`.
fn throughput_kibs(bytes: usize, elapsed: Duration) -> f64 {
    (bytes as f64 / 1024.0) / elapsed.as_secs_f64().max(1e-9)
}

/// Why the reader stopped.
///
/// A transfer that returned nothing and a transfer that ran slowly are different findings, and a
/// bare timeout cannot tell them apart. Naming the reason is what lets a scenario fail immediately
/// on "the exit stopped serving" instead of waiting out a deadline that can no longer teach it
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpOutcome {
    /// The whole payload came back.
    Complete,
    /// Bytes arrived and then the return stream went quiet for [`READ_IDLE_TIMEOUT`].
    Idle,
    /// Nothing ever came back within [`NO_FIRST_BYTE_TIMEOUT`].
    NeverStarted,
    /// The session reported end-of-stream — the counterparty stopped serving it.
    SessionClosed,
    /// The overall deadline expired while the stream was still delivering.
    DeadlineExceeded,
    /// Everything had been offered and the reader's grace period after that expired.
    ///
    /// Distinct from [`Self::Idle`]: the stream was still arriving, just far too slowly to finish.
    /// Whatever had not come back by then is loss, and waiting longer only measures the size of a
    /// backlog rather than the health of the return path.
    TailGraceExpired,
}

impl PumpOutcome {
    /// Whether the counterparty stopped serving the session, as opposed to serving it badly.
    ///
    /// Both of these mean no further waiting can change the answer: there is nothing on the other
    /// end to recover.
    pub fn exit_stopped_serving(&self) -> bool {
        matches!(self, Self::NeverStarted | Self::SessionClosed)
    }
}

/// Size of one tagged payload record. See [`tagged_payload`].
pub const RECORD_SIZE: usize = 16;

/// Marks the start of a record, so arrivals can be attributed without assuming alignment.
const RECORD_MAGIC: [u8; 4] = *b"HPRT";

/// Builds a `bytes`-long payload out of records stamped with `phase` and a running index.
///
/// Two phases running over one session cannot be told apart by volume alone. When a burst arrives
/// late, "the second phase finally worked" and "the first phase's buffer released" produce the same
/// byte count, and the wrong one of those reads as recovery. Stamping the bytes makes it a fact
/// rather than an inference: 2.3 MB arriving in the last three seconds of a run is a completely
/// different finding depending on which phase sent it.
///
/// Layout per record: `MAGIC(4) | phase(1) | reserved(3) | index u64 LE(8)`.
pub fn tagged_payload(phase: u8, bytes: usize) -> Vec<u8> {
    let records = bytes / RECORD_SIZE;
    let mut payload = Vec::with_capacity(records * RECORD_SIZE);
    for index in 0..records as u64 {
        payload.extend_from_slice(&RECORD_MAGIC);
        payload.push(phase);
        payload.extend_from_slice(&[0u8; 3]);
        payload.extend_from_slice(&index.to_le_bytes());
    }
    payload
}

/// Counts whole records in `buf[from..]`, splitting them by whether they carry `phase`.
///
/// Returns the position to resume from, so a caller reading a stream can scan incrementally and
/// still catch a record straddling two reads: scanning stops at the first offset where a whole
/// record no longer fits, and the next call picks up exactly there.
fn scan_records(
    buf: &[u8],
    from: usize,
    phase: u8,
    mine: &mut usize,
    foreign: &mut usize,
    mut mine_buf: Option<&mut Vec<u8>>,
) -> usize {
    let mut at = from;
    while at + RECORD_SIZE <= buf.len() {
        if buf[at..at + 4] == RECORD_MAGIC {
            if buf[at + 4] == phase {
                *mine += 1;
                if let Some(out) = mine_buf.as_deref_mut() {
                    out.extend_from_slice(&buf[at..at + RECORD_SIZE]);
                }
            } else {
                *foreign += 1;
            }
            at += RECORD_SIZE;
        } else {
            at += 1;
        }
    }
    at
}

/// How a pump offers its payload and what it counts as its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct PumpOpts {
    /// Per-chunk send delay; `None` offers data as fast as the session accepts it.
    pub pace: Option<Duration>,
    /// Phase tag whose records count toward this transfer; `None` counts every byte received.
    pub phase: Option<u8>,
    /// Quiet period after which the stream counts as idle; `None` uses [`READ_IDLE_TIMEOUT`].
    ///
    /// Must be stated by any caller measuring how long a stream takes to come back, and must
    /// exceed that deadline. The default is shorter than the return-path recovery deadline, so a
    /// recovery slower than it would end the pump before it could be observed -- and for a phased
    /// pump that has received nothing yet, it ends as `NeverStarted`, which reads as "the exit
    /// stopped serving" while the exit is still visibly serving the other phase on the same
    /// socket. The budget and the deadline live in different files, so state it here rather than
    /// leave them to drift.
    pub idle_budget: Option<Duration>,
    /// How long to keep reading after the last byte has been *offered*; `None` waits indefinitely
    /// (subject to the overall deadline and the idle budget).
    ///
    /// The idle budget cannot bound a stream that trickles: a session returning a few bytes a
    /// second is never quiet long enough to look idle and never fast enough to finish, so the
    /// pump runs until the overall deadline or until the exit gives up. One run spent 444 s that
    /// way to offer 60 s of data, and the seven minutes it added measured a draining backlog, not
    /// the return path. Stating a grace here turns "how long until everything eventually shows up"
    /// into "how much came back in a fixed window", which is the question a survival scenario is
    /// actually asking.
    pub tail_grace: Option<Duration>,
}

/// Result of one loopback round-trip.
#[derive(Debug, Clone)]
pub struct Transfer {
    /// Why the reader stopped.
    pub outcome: PumpOutcome,
    pub sent_bytes: usize,
    /// Every byte read back, whichever phase sent it.
    pub received_bytes: usize,
    /// Bytes carrying this transfer's own phase tag, or all of them when untagged.
    ///
    /// This — not [`Self::received_bytes`] — is what every rate and recovery figure is computed
    /// from, so a backlog released by an earlier phase cannot be read as this one recovering.
    pub attributed_bytes: usize,
    /// Bytes carrying some *other* phase's tag: an earlier phase's backlog arriving late.
    pub foreign_bytes: usize,
    /// Wall-clock from first byte written to last byte read back.
    pub seconds: f64,
    /// Goodput = [`Self::attributed_bytes`] / [`Self::seconds`], in MB/s.
    ///
    /// Spans first arrival to last, so a few late stragglers stretch the denominator after the bulk
    /// has already landed: three cluster runs delivering the same 96 % reported 0.22, 0.22 and
    /// 0.29 MB/s on that basis. Prefer [`Self::throughput_at`] for a figure a single packet cannot
    /// move.
    pub mbps: f64,
    /// True only when the full payload returned intact (received == sent and bytes match).
    pub sha_ok: bool,
    /// Wall-clock from the moment the pump started to the moment **the reader** stopped.
    ///
    /// Distinct from [`Self::seconds`], which spans first byte to last: the difference between the
    /// two is exactly the time a recovering stream spent delivering nothing, which is the interval
    /// a recovery deadline is about.
    ///
    /// Deliberately not the moment the pump returns. The writer is bounded separately, and a
    /// backpressured one can keep running long after the reader has stopped; charging that to the
    /// stream would make [`Self::longest_stall`] report the writer as return-path silence -- and
    /// that value is asserted against the recovery deadline.
    pub wall_seconds: f64,
    /// `(seconds since the *pump started*, cumulative bytes received)`, one per read.
    ///
    /// Aggregate arrival cannot express *when* a transfer recovered — it folds recovery latency,
    /// steady-state rate and timeout behaviour into a single number whose run-to-run spread is
    /// wider than the effects worth measuring. The series keeps the shape so recovery can be read
    /// off it directly.
    ///
    /// Stamped from the pump start rather than the first arrival, so a stream that delivered
    /// nothing for its first 12 s cannot report that it recovered at t=0.
    pub progress: Vec<(f64, usize)>,
}

impl Transfer {
    /// Percent of sent bytes that returned *carrying this phase's tag*.
    ///
    /// Deliberately not [`Self::received_bytes`]: on a session shared with an earlier phase, a
    /// backlog released late would otherwise inflate this figure into a recovery that never was.
    pub fn arrival_pct(&self) -> f64 {
        (self.attributed_bytes as f64) / (self.sent_bytes.max(1) as f64) * 100.0
    }

    /// Seconds from the start of the pump to the first byte back, `None` if none ever arrived.
    pub fn time_to_first_byte(&self) -> Option<f64> {
        self.progress.first().map(|&(at, _)| at)
    }

    /// The longest interval in which no bytes arrived.
    ///
    /// Counts the wait for the first byte and the tail after the last one, not merely the gaps
    /// between arrivals: a stream that delivered a burst and then died has its worst stall
    /// entirely in that tail, and a measure that ignored it would call the burst healthy.
    pub fn longest_stall(&self) -> f64 {
        let mut previous = 0.0;
        let mut worst: f64 = 0.0;
        for &(at, _) in &self.progress {
            worst = worst.max(at - previous);
            previous = at;
        }
        // The silence after the last arrival counts only when the pump was still expecting data.
        // `Idle` is *defined* by a trailing gap of exactly the idle budget, so counting it reports
        // the detector's own threshold as the symptom -- and since that budget is required to
        // exceed the recovery deadline it measures, every idle-terminated run would fail a
        // deadline check it structurally cannot pass. One run delivered 97.8% and recovered in
        // 13.8s, then failed for a "30.1s stall" that was the pump waiting to be sure the stream
        // had ended.
        if self.outcome == PumpOutcome::Idle {
            return worst;
        }
        worst.max(self.wall_seconds - previous)
    }

    /// Throughput up to the moment `fraction` of the offered payload had come back, in MB/s.
    ///
    /// `None` if that share never arrived.
    ///
    /// [`Self::mbps`] divides everything received by first-arrival-to-last-arrival, which makes it
    /// hostage to the tail: the same session delivering the same 96 % reported 0.22 MB/s on two
    /// runs and 0.29 MB/s on a third, purely because a few late stragglers stretched the window
    /// after the bulk had already landed. That is a statistic a single packet can move.
    ///
    /// Measuring to a share of the payload cuts the tail out. The denominator runs from the pump's
    /// start, not from the first byte, so the outage the session actually suffered is charged to
    /// it -- this is delivered throughput, not throughput-while-delivering.
    pub fn throughput_at(&self, fraction: f64) -> Option<f64> {
        let target = (self.sent_bytes as f64 * fraction.clamp(0.0, 1.0)).ceil() as usize;
        self.progress
            .iter()
            .find(|&&(_, bytes)| bytes >= target && target > 0)
            .map(|&(at, bytes)| bytes as f64 / 1_000_000.0 / at.max(1e-9))
    }

    /// Gap between consecutive arrivals at quantile `q`, `None` with fewer than two arrivals.
    ///
    /// The pair p50/p95 separates a stream that is merely slow (both rise together) from one that
    /// is stuttering (p50 stays low while p95 blows out) — a distinction that mean throughput
    /// cannot make.
    pub fn inter_arrival_quantile(&self, q: f64) -> Option<f64> {
        if self.progress.len() < 2 {
            return None;
        }
        let mut gaps: Vec<f64> = self.progress.windows(2).map(|w| w[1].0 - w[0].0).collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).expect("arrival timestamps are never NaN"));
        let index = (((gaps.len() - 1) as f64) * q.clamp(0.0, 1.0)).round() as usize;
        gaps.get(index).copied()
    }

    /// Throughput over the final `window` of the pump, in MB/s.
    ///
    /// Anchored on the end of the *pump*, not on the last arrival. A stream that died has no
    /// arrivals to anchor on, and anchoring on its last one would report the rate it managed
    /// before dying — which is precisely how a dead stream came to be reported as recovered.
    pub fn steady_state_mbps(&self, window: Duration) -> f64 {
        let window_s = window.as_secs_f64();
        let from = self.wall_seconds - window_s;
        if from < 0.0 {
            return 0.0;
        }
        let Some(&(_, total)) = self.progress.last() else {
            return 0.0;
        };
        let at_from = self
            .progress
            .iter()
            .rev()
            .find(|(at, _)| *at <= from)
            .map(|&(_, bytes)| bytes)
            .unwrap_or(0);
        ((total.saturating_sub(at_from)) as f64) / 1_000_000.0 / window_s
    }

    /// Seconds until throughput first sustains `target_mbps` over a `window`.
    ///
    /// This is the statistic a recovery target is actually about: how long the transfer stayed
    /// degraded, not how much arrived in total. `None` means it never got there.
    ///
    /// A window that has not yet elapsed cannot have been sustained, so no answer is returned
    /// before `window` and none at all for a pump shorter than one. Without that, an opening burst
    /// satisfies the window on its own and a transfer that died immediately afterwards reports a
    /// recovery that never happened.
    pub fn time_to_sustain(&self, target_mbps: f64, window: Duration) -> Option<f64> {
        let window_s = window.as_secs_f64();
        if self.wall_seconds < window_s {
            return None;
        }
        let need = target_mbps * 1_000_000.0 * window_s;

        // For each sample, look back one window; the first point where enough arrived within it is
        // when the transfer was last still degraded.
        let mut earliest = 0usize;
        for (i, &(at, bytes)) in self.progress.iter().enumerate() {
            if at < window_s {
                continue;
            }
            while self.progress[earliest].0 < at - window_s {
                earliest += 1;
            }
            if earliest == i {
                continue;
            }
            if (bytes - self.progress[earliest].1) as f64 >= need {
                return Some(at);
            }
        }
        None
    }
}

/// Bytes handed to the session per `write_all`.
///
/// Bounded by the largest datagram a `NoDelay` session accepts, not by a round 64 KiB.
/// hoprnet #8358 made `NoDelay` imply datagram-boundary preservation on stateless sessions:
/// each write becomes exactly one frame, and `segment_into` rejects a frame needing more than
/// `SeqIndicator::MAX` segments of `SESSION_MTU` bytes — `SeqIndicator::try_from` returns
/// `GeneralError::InvalidInput`, which surfaces as "input argument to the function is invalid".
/// At the current 1020-byte MTU a 64 KiB write needs 65 segments, so every send failed
/// outright: the session established, then nothing was ever transmitted (`outcome
/// NeverStarted`, 0% arrival, on both hop counts).
///
/// The multiplier is 63, not 64. Upstream's doc comment says a datagram "may be as large as
/// `64 * SESSION_MTU`", but `SeqIndicator::MAX` IS 63 and `try_from` rejects anything above it,
/// so 64 segments is already one too many. Confirmed by A/B on one machine at CI's exact
/// dependency revisions: `64 * SESSION_MTU` reproduces the 0% failure, `63 * SESSION_MTU` passes.
///
/// `SeqIndicator` is not re-exported through hopr-lib, so 63 is spelled out here; `SESSION_MTU`
/// is imported, so this still tracks a packet-size change.
const IO_CHUNK: usize = 63 * SESSION_MTU;

/// Per-chunk delay to cap the send rate at `HOPRD_PUMP_MBPS` MB/s. Blasting a large
/// payload saturates the node's rayon packet pool on CPU-constrained CI runners
/// (decode timeouts → heavy loss). Unset or ≤0 = unpaced.
fn send_pace_per_chunk() -> Option<Duration> {
    let mbps: f64 = std::env::var("HOPRD_PUMP_MBPS").ok()?.parse().ok()?;
    if mbps <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(
        IO_CHUNK as f64 / (mbps * 1_000_000.0),
    ))
}
/// If no bytes arrive for this long after the first byte, the return transfer is
/// considered finished (UDP loopback gives no EOF; lost tail bytes never arrive).
pub const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the *first* byte back before concluding nothing is coming.
///
/// The idle rule needs an arrival to anchor on, so before anything has come back it cannot fire and
/// the reader would otherwise wait out the whole deadline. Generous against a recovery target
/// measured in tens of seconds, but far short of the deadline: a return stream that has produced
/// nothing by now has already failed, and the remaining minutes only cost wall-clock.
pub const NO_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the reader re-evaluates whether to keep waiting.
const READ_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The writer's completion as the reader sees it.
///
/// `None` at the call site means the offer is still in flight, or that the caller set no grace —
/// either way the cap cannot fire yet, which keeps "has the offer finished?" out of the rule.
#[derive(Debug, Clone, Copy)]
struct TailWindow {
    /// How long ago the last byte was handed to the session.
    since_offer_complete: Duration,
    /// How long the reader keeps reading after that.
    grace: Duration,
}

/// Sentinel for "the writer has not finished offering yet" in the shared stamp.
const OFFER_IN_FLIGHT: u64 = u64::MAX;

/// Whether the reader should stop, and why; `None` to keep waiting.
///
/// Pure so the stopping rule can be exercised without a session or a cluster — the rule is what
/// went wrong (a run spent seven minutes measuring a session whose server side had already ended),
/// not the plumbing around it.
fn stop_reason(
    received: usize,
    since_last_arrival: Duration,
    since_start: Duration,
    deadline: Duration,
    idle_budget: Duration,
    tail: Option<TailWindow>,
) -> Option<PumpOutcome> {
    if since_start >= deadline {
        return Some(PumpOutcome::DeadlineExceeded);
    }
    // A hard cap, and deliberately ahead of every rule below it: once the last byte has been
    // offered the reader waits out the grace and stops, whatever the stream is doing. The idle
    // rule cannot express this -- a trickling stream is never quiet -- so without the cap a
    // session returning a few bytes a second runs to the overall deadline and the run reports how
    // long a backlog took to drain rather than whether the return path recovered.
    if tail.is_some_and(|t| t.since_offer_complete >= t.grace) {
        // The cap only decides *when* to stop asking. If nothing ever came back, the exit never
        // served this phase, and that -- not the cap -- is the finding worth reporting.
        return Some(if received == 0 {
            PumpOutcome::NeverStarted
        } else {
            PumpOutcome::TailGraceExpired
        });
    }
    if received == 0 {
        // Nothing has arrived at all, so the idle rule has nothing to measure from and the
        // first-byte budget decides instead.
        return (since_start >= idle_budget.max(NO_FIRST_BYTE_TIMEOUT))
            .then_some(PumpOutcome::NeverStarted);
    }
    (since_last_arrival >= idle_budget).then_some(PumpOutcome::Idle)
}

pub fn sha256_digest(data: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

/// Pump `payload` through `session` to the exit-node loopback and measure return
/// goodput + loss.
///
/// Consumes the session. Use [`pump_halves`] when the same session has to survive more than one
/// pump.
pub async fn pump_loopback(
    session: HoprSession,
    payload: &[u8],
    label: &str,
    timeout: Duration,
) -> anyhow::Result<Transfer> {
    let (mut rx, mut tx) = tokio::io::split(session);
    pump_halves(
        &mut rx,
        &mut tx,
        payload,
        label,
        timeout,
        PumpOpts::default(),
    )
    .await
}

/// Per-chunk delay that offers `payload_bytes` at `mbps` MB/s.
///
/// Used to make a phase's offered load *last*. A payload handed to the session as fast as it will
/// take it is fully committed within seconds, so anything measured afterwards is a buffer draining
/// rather than a session working — and a recovery that takes longer than that has nothing left to
/// demonstrate itself on.
pub fn pace_for_rate(mbps: f64) -> Option<Duration> {
    (mbps > 0.0).then(|| Duration::from_secs_f64(IO_CHUNK as f64 / (mbps * 1_000_000.0)))
}

/// Upper bound on a single drain, however busy the stream stays.
///
/// Without it the loop only ends on quiet or end-of-stream, so a return stream that keeps
/// delivering holds the caller indefinitely -- and the scenario has no other guard before its
/// next phase, so the run would hang rather than fail.
const DRAIN_MAX_TOTAL: Duration = Duration::from_secs(60);

/// Reads and discards whatever is still arriving until the stream is quiet for `quiet_for`.
///
/// Between two measured phases on one session this is what stops the first phase's tail being
/// counted as the second phase's arrival. The returned byte count is itself a finding: a large
/// number means the previous phase had not actually finished when it was declared complete.
pub async fn drain_until_quiet(
    rx: &mut tokio::io::ReadHalf<HoprSession>,
    quiet_for: Duration,
    label: &str,
) -> usize {
    let mut buf = vec![0u8; IO_CHUNK];
    let mut discarded = 0usize;
    let deadline = tokio::time::Instant::now() + DRAIN_MAX_TOTAL;
    let mut capped = false;

    while let Ok(Ok(read)) = tokio::time::timeout(quiet_for, rx.read(&mut buf)).await {
        if read == 0 {
            break;
        }
        discarded += read;
        if tokio::time::Instant::now() >= deadline {
            capped = true;
            break;
        }
    }

    if capped {
        tracing::warn!(
            "{label}: drain hit the {DRAIN_MAX_TOTAL:?} cap after {discarded} B; the stream never \
             went quiet, so the next phase may still see earlier-phase traffic"
        );
    } else {
        tracing::info!(
            "{label}: drained {discarded} B of leftover return traffic before the next phase"
        );
    }
    discarded
}

/// Pump `payload` to the exit-node loopback over *borrowed* session halves and measure return
/// goodput + loss.
///
/// The writer pushes the payload as the session sink accepts it (bounded-buffer
/// backpressure + the session's SURB egress cap pace it). The reader accumulates
/// returned bytes until it has the full payload back or the return stream goes idle
/// (UDP loss means the tail may never arrive). Goodput = received_bytes /
/// (first→last byte). `sha_ok` is only asserted on a lossless round-trip.
///
/// Does NOT `shutdown()` the write half: HOPR sessions have no TCP half-close.
///
/// Borrowing rather than consuming is what lets a scenario measure the *same* session before and
/// after a fault. Opening a replacement session instead measures cold-start path selection, which
/// is a different question and — for a return relayer that has just died — a much easier one: the
/// new session never minted a SURB through the dead node.
///
/// The writer runs concurrently with the reader instead of on a spawned task, because a spawned
/// task owns its half and an aborted one drops it, so `unsplit` could not reliably recover the
/// session afterwards.
pub async fn pump_halves(
    rx: &mut tokio::io::ReadHalf<HoprSession>,
    tx: &mut tokio::io::WriteHalf<HoprSession>,
    payload: &[u8],
    label: &str,
    timeout: Duration,
    opts: PumpOpts,
) -> anyhow::Result<Transfer> {
    let expected = sha256_digest(payload);
    let total_bytes = payload.len();
    let pace = opts.pace.or_else(send_pace_per_chunk);

    // Everything is stamped against the moment the pump started, not the first byte back. On a
    // recovering stream the interval between the two *is* the outage, and timing from the first
    // arrival discards exactly the quantity a recovery deadline is about.
    let pump_started = std::time::Instant::now();
    // When the writer finished offering, shared with the reader so the tail cap has something to
    // measure from. Written once by the writer, read on the reader's poll cadence.
    let offer_completed_ms = std::sync::atomic::AtomicU64::new(OFFER_IN_FLIGHT);

    let send = async {
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + IO_CHUNK).min(payload.len());
            tx.write_all(&payload[offset..end]).await?;
            if let Some(d) = pace {
                tokio::time::sleep(d).await;
            }
            offset = end;
        }
        tx.flush().await?;
        // After the flush, so the cap starts from the last byte actually handed over rather than
        // from the last one queued.
        offer_completed_ms.store(
            pump_started.elapsed().as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok::<_, std::io::Error>(())
    };

    let mut received: Vec<u8> = Vec::with_capacity(total_bytes);
    let mut buf = vec![0u8; IO_CHUNK];
    let mut progress: Vec<(f64, usize)> = Vec::new();
    let mut first_at: Option<std::time::Instant> = None;
    let mut last_at = pump_started;
    // Liveness for a phased pump is about *this* phase. An earlier phase draining keeps the raw
    // stream busy and `last_at` fresh, so neither `NeverStarted` nor `Idle` could fire while this
    // phase received nothing -- the pump would burn the whole timeout and report
    // `DeadlineExceeded` instead of the fast, accurate answer.
    let mut last_mine_at = pump_started;
    let idle_budget = opts.idle_budget.unwrap_or(READ_IDLE_TIMEOUT);
    // When the *reader* stopped, which is not when the pump returns. The writer is bounded
    // separately and a backpressured one can outlive the reader by minutes; measuring to the join
    // would charge that time to the return path.
    let mut reader_stopped_at: Option<std::time::Instant> = None;

    // Attribution is incremental: rescanning the whole buffer after every read would be quadratic
    // over a payload measured in megabytes.
    let mut scan_at = 0usize;
    let mut mine = 0usize;
    let mut foreign = 0usize;
    // Records belonging to this phase, in arrival order, so integrity is checked against what
    // this phase actually sent rather than against a stream carrying another phase's backlog.
    let mut mine_buf: Vec<u8> = Vec::new();
    let mut outcome = PumpOutcome::Complete;
    let recv = async {
        loop {
            // Completion is per phase. Counting the raw stream lets a released backlog from an
            // earlier phase fill the budget, so the pump would report `Complete` before its own
            // records had all arrived -- and the byte count it reported would not be its own.
            let done = match opts.phase {
                Some(_) => mine * RECORD_SIZE >= total_bytes,
                None => received.len() >= total_bytes,
            };
            if done {
                outcome = PumpOutcome::Complete;
                break;
            }
            let now = std::time::Instant::now();
            let (live_bytes, since_live) = match opts.phase {
                Some(_) => (
                    mine * RECORD_SIZE,
                    now.saturating_duration_since(last_mine_at),
                ),
                None => (received.len(), now.saturating_duration_since(last_at)),
            };
            let tail = opts.tail_grace.and_then(|grace| {
                match offer_completed_ms.load(std::sync::atomic::Ordering::Relaxed) {
                    OFFER_IN_FLIGHT => None,
                    at => Some(TailWindow {
                        since_offer_complete: now
                            .saturating_duration_since(pump_started)
                            .saturating_sub(Duration::from_millis(at)),
                        grace,
                    }),
                }
            });
            if let Some(stop) = stop_reason(
                live_bytes,
                since_live,
                now.saturating_duration_since(pump_started),
                timeout,
                idle_budget,
                tail,
            ) {
                tracing::warn!(
                    "{label}: stopping ({stop:?}) at {}/{total_bytes} B",
                    received.len()
                );
                outcome = stop;
                break;
            }
            // Poll rather than block for the whole idle budget, so `stop_reason` -- which owns
            // every stopping decision -- is re-evaluated on a fixed cadence.
            match tokio::time::timeout(READ_POLL_INTERVAL, rx.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    tracing::warn!(
                        "{label}: counterparty closed the session at {}/{total_bytes} B",
                        received.len()
                    );
                    outcome = PumpOutcome::SessionClosed;
                    break;
                }
                Ok(Ok(just_read)) => {
                    last_at = std::time::Instant::now();
                    first_at.get_or_insert(last_at);
                    received.extend_from_slice(&buf[..just_read]);
                    let attributed = match opts.phase {
                        Some(phase) => {
                            let before = mine;
                            scan_at = scan_records(
                                &received,
                                scan_at,
                                phase,
                                &mut mine,
                                &mut foreign,
                                Some(&mut mine_buf),
                            );
                            if mine > before {
                                last_mine_at = last_at;
                            }
                            mine * RECORD_SIZE
                        }
                        None => received.len(),
                    };
                    progress.push((
                        last_at
                            .saturating_duration_since(pump_started)
                            .as_secs_f64(),
                        attributed,
                    ));
                }
                Ok(Err(e)) => return Err(anyhow::anyhow!("{label}: read error: {e}")),
                Err(_) => continue,
            }
        }
        reader_stopped_at = Some(std::time::Instant::now());
        Ok(())
    };

    // The reader owns the stopping condition; the writer is bounded only so a wedged sink cannot
    // outlive the scenario. Both borrow, so neither can be aborted out from under the session.
    let (sent, read_outcome) = tokio::join!(tokio::time::timeout(timeout, send), recv);
    read_outcome?;
    match sent {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("{label}: sender error: {e}"),
        Err(_) => tracing::warn!("{label}: sender did not finish within {timeout:?}"),
    }

    let first_at = first_at.unwrap_or(last_at);
    let seconds = last_at
        .saturating_duration_since(first_at)
        .as_secs_f64()
        .max(1e-9);
    // To the reader's own stop, per this field's contract. Falling back to now only covers the
    // read-error path, which returns before the measurement is used.
    let wall_seconds = reader_stopped_at
        .unwrap_or_else(std::time::Instant::now)
        .saturating_duration_since(pump_started)
        .as_secs_f64()
        .max(1e-9);
    let received_bytes = received.len();
    let (attributed_bytes, foreign_bytes) = match opts.phase {
        Some(_) => (mine * RECORD_SIZE, foreign * RECORD_SIZE),
        None => (received_bytes, 0),
    };
    let mbps = (attributed_bytes as f64) / 1_000_000.0 / seconds;
    // A phased pump's raw buffer interleaves another phase's records, so hashing it can never
    // match. Check the records this phase actually sent.
    let sha_ok = match opts.phase {
        Some(_) => mine_buf.len() == total_bytes && sha256_digest(&mine_buf) == expected,
        None => received_bytes == total_bytes && sha256_digest(&received) == expected,
    };

    let transfer = Transfer {
        outcome,
        sent_bytes: total_bytes,
        received_bytes,
        attributed_bytes,
        foreign_bytes,
        seconds,
        mbps,
        sha_ok,
        wall_seconds,
        progress,
    };

    tracing::info!(
        "{label}: recv {received_bytes}/{total_bytes} B in {seconds:.2}s = {mbps:.2} MB/s \
         (wall {wall_seconds:.2}s), attributed {attributed_bytes} B, foreign {foreign_bytes} B, \
         arrival {:.2}%, outcome {outcome:?}, ttfb {}, \
         longest stall {:.2}s, inter-arrival p50 {} / p95 {}, sha_ok={sha_ok}",
        transfer.arrival_pct(),
        transfer
            .time_to_first_byte()
            .map_or("never".to_string(), |s| format!("{s:.2}s")),
        transfer.longest_stall(),
        transfer
            .inter_arrival_quantile(0.5)
            .map_or("n/a".to_string(), |s| format!("{s:.3}s")),
        transfer
            .inter_arrival_quantile(0.95)
            .map_or("n/a".to_string(), |s| format!("{s:.3}s")),
    );

    Ok(transfer)
}

/// Pump `payload` with a **single `write_all`** — no batching, no inter-batch
/// sleep, no cooperative yield. The write task submits every packet to the session
/// sink without ever returning `Poll::Pending`, monopolising one tokio worker thread.
/// This is the production anti-pattern (`transfer_session` / `copy_duplex` over a fast
/// source) the executor-starvation profiling harness makes visible: with the SURB
/// balancer starved, the return path stalls and goodput collapses. See
/// `tests/profiling.rs`.
///
/// A read timeout is logged rather than returned as an error, so the profiling run
/// still completes and writes its trace even when the payload stalls mid-flight.
///
/// **Broken against hoprnet #8358 for payloads over `IO_CHUNK`.** That change made
/// `NoDelay` imply datagram-boundary preservation, so this single `write_all` is offered
/// as one datagram and rejected once it exceeds 64 segments (see [`IO_CHUNK`]). Chunking it
/// is not the fix — the unbroken write is the anti-pattern being measured — so reviving this
/// harness needs either a smaller payload or a session without `NoDelay`. Only reachable
/// from the `prof`-gated, `#[ignore]`d `tests/profiling.rs`, so CI is unaffected.
pub async fn pump_continuous(
    session: HoprSession,
    payload: &[u8],
    label: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let (mut r, mut w) = tokio::io::split(session);
    let payload_bytes = payload.to_vec();
    let expected = sha256_digest(payload);
    let n = payload.len();
    let start = std::time::Instant::now();

    let writer = tokio::spawn(async move {
        w.write_all(&payload_bytes).await?;
        w.flush().await?;
        Ok::<_, std::io::Error>(())
    });

    let mut received = vec![0u8; n];
    let read_result = tokio::time::timeout(timeout, r.read_exact(&mut received)).await;

    let elapsed = start.elapsed();
    let kibs = throughput_kibs(n, elapsed);

    match read_result {
        Ok(Ok(_)) => {
            // Confirm the writer finished cleanly and the bytes are intact *before* logging
            // success, so a writer error or SHA mismatch never rides under a ✓ line.
            writer
                .await
                .map_err(|e| anyhow::anyhow!("{label}: writer panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("{label}: write error: {e}"))?;
            anyhow::ensure!(
                sha256_digest(&received) == expected,
                "{label}: SHA-256 mismatch — {n} bytes corrupted in transit"
            );
            tracing::info!("{label}: ✓ {n} B in {elapsed:.2?} ({kibs:.0} KiB/s) — continuous");
        }
        Ok(Err(e)) => {
            writer.abort();
            let _ = writer.await;
            anyhow::bail!("{label}: read error: {e}");
        }
        Err(_timeout) => {
            writer.abort();
            let _ = writer.await;
            // Not a hard error — the stall is the observation. `read_exact` reports no
            // partial count on timeout, so no fabricated throughput figure is logged.
            tracing::warn!(
                "{label}: read timeout ({timeout:?}) after {elapsed:.2?} — the {n} B payload did \
                 not fully arrive. Expected under executor starvation: the single write_all held a \
                 tokio worker thread without yielding, starving the SURB balancer."
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transfer with the given `(second, cumulative bytes)` arrivals over a `wall_seconds` pump.
    fn transfer(wall_seconds: f64, progress: &[(f64, usize)]) -> Transfer {
        let received_bytes = progress.last().map(|&(_, b)| b).unwrap_or(0);
        Transfer {
            outcome: PumpOutcome::Complete,
            sent_bytes: received_bytes.max(1),
            received_bytes,
            attributed_bytes: received_bytes,
            foreign_bytes: 0,
            seconds: progress
                .last()
                .zip(progress.first())
                .map(|((last, _), (first, _))| last - first)
                .unwrap_or(0.0),
            mbps: 0.0,
            sha_ok: false,
            wall_seconds,
            progress: progress.to_vec(),
        }
    }

    /// One megabyte per arrival, one arrival per second, from `from` to `to` inclusive.
    fn steady(from: u64, to: u64) -> Vec<(f64, usize)> {
        (from..=to)
            .map(|s| (s as f64, ((s - from + 1) as usize) * 1_000_000))
            .collect()
    }

    /// The measurement that reported "recovered after 381.3s" for a stream that was dead.
    ///
    /// A burst delivers well over the target, the stream then delivers nothing for the rest of the
    /// pump. Anchoring on arrivals alone, the burst satisfies the window and the transfer looks
    /// recovered; the window has to be anchored in wall-clock for the burst to be seen for what it
    /// is.
    #[test]
    fn a_burst_followed_by_silence_should_not_read_as_recovery() {
        let burst = transfer(380.0, &steady(0, 2));

        assert_eq!(
            burst.time_to_sustain(0.5, Duration::from_secs(3)),
            None,
            "an opening burst is not a sustained rate"
        );
        assert_eq!(
            burst.steady_state_mbps(Duration::from_secs(3)),
            0.0,
            "the final window of a dead stream carries nothing"
        );
        assert!(
            burst.longest_stall() > 370.0,
            "the tail after the last byte is the stall, got {:.1}s",
            burst.longest_stall()
        );
    }

    /// Vacuity guard for the above: the same shape sustained to the end must read as recovered.
    #[test]
    fn a_rate_held_to_the_end_should_read_as_recovery() {
        let healthy = transfer(10.0, &steady(0, 10));

        assert!(
            healthy
                .time_to_sustain(0.5, Duration::from_secs(3))
                .is_some_and(|s| s <= 4.0),
            "a rate held throughout should recover promptly, got {:?}",
            healthy.time_to_sustain(0.5, Duration::from_secs(3))
        );
        assert!(
            healthy.steady_state_mbps(Duration::from_secs(3)) >= 0.9,
            "the final window should carry ~1 MB/s, got {:.2}",
            healthy.steady_state_mbps(Duration::from_secs(3))
        );
    }

    /// Recovery is measured from the moment the pump starts, because that is when the fault the
    /// stream is recovering from was introduced. Timing from the first byte back silently discards
    /// the entire outage.
    #[test]
    fn recovery_should_be_timed_from_the_pump_start_not_the_first_byte() {
        let late = transfer(30.0, &steady(12, 30));

        let recovered = late
            .time_to_sustain(0.5, Duration::from_secs(3))
            .expect("the stream does sustain the rate once it starts");
        assert!(
            recovered >= 12.0,
            "the 12s outage before the first byte must be counted, got {recovered:.1}s"
        );
        assert_eq!(late.time_to_first_byte(), Some(12.0));
    }

    /// A pump shorter than the sustain window cannot answer the question at all.
    #[test]
    fn a_pump_shorter_than_the_window_should_report_no_recovery() {
        let brief = transfer(1.6, &[(0.0, 4_000_000), (1.6, 8_000_000)]);

        assert_eq!(
            brief.time_to_sustain(0.5, Duration::from_secs(3)),
            None,
            "a 1.6s pump cannot demonstrate a 3s sustained rate"
        );
    }

    /// The wait for the first byte is a stall like any other, and on a recovering stream it is
    /// usually the longest one.
    #[test]
    fn the_wait_for_the_first_byte_should_count_as_a_stall() {
        let late = transfer(20.0, &steady(9, 20));

        assert!(
            (late.longest_stall() - 9.0).abs() < 1e-9,
            "the 9s opening gap is the longest stall, got {:.1}s",
            late.longest_stall()
        );
    }

    /// A stuttering stream and a uniformly slow one can carry identical totals; the p50/p95 pair is
    /// what tells them apart.
    #[test]
    fn inter_arrival_quantiles_should_separate_stutter_from_slowness() {
        let stuttering = transfer(
            20.0,
            &[
                (0.0, 1_000_000),
                (0.1, 2_000_000),
                (0.2, 3_000_000),
                (0.3, 4_000_000),
                (12.0, 5_000_000),
            ],
        );
        let p50 = stuttering.inter_arrival_quantile(0.5).expect("has gaps");
        let p95 = stuttering.inter_arrival_quantile(0.95).expect("has gaps");

        assert!(p50 <= 0.2, "typical gap should stay small, got {p50:.2}s");
        assert!(
            p95 > 10.0,
            "the outlier gap must show at p95, got {p95:.2}s"
        );

        let smooth = transfer(5.0, &steady(0, 5));
        assert_eq!(
            smooth.inter_arrival_quantile(0.5),
            smooth.inter_arrival_quantile(0.95),
            "an evenly-paced stream has no spread between p50 and p95"
        );
    }

    // ── payload attribution ───────────────────────────────────────────────────

    /// The property the whole two-phase measurement rests on: a byte can be traced to the phase
    /// that sent it, so a backlog released late is never counted as the later phase recovering.
    #[test]
    fn records_should_be_attributed_to_the_phase_that_sent_them() {
        let mut buf = tagged_payload(1, 4 * RECORD_SIZE);
        buf.extend_from_slice(&tagged_payload(2, 6 * RECORD_SIZE));

        let (mut mine, mut foreign) = (0, 0);
        scan_records(&buf, 0, 2, &mut mine, &mut foreign, None);
        assert_eq!(mine, 6, "phase 2 sent six records");
        assert_eq!(
            foreign, 4,
            "the other four belong to phase 1 and must not be credited"
        );
    }

    /// The collected buffer must survive a record straddling two reads, since that is the normal
    /// case on a live stream — the single-scan tests would not catch an off-by-one there.
    #[test]
    fn the_collected_buffer_should_be_intact_across_a_read_boundary() {
        let payload = tagged_payload(2, 4 * RECORD_SIZE);
        let split = RECORD_SIZE + 5; // lands mid-record

        let (mut mine, mut foreign) = (0, 0);
        let mut mine_buf = Vec::new();
        let resume = scan_records(
            &payload[..split],
            0,
            2,
            &mut mine,
            &mut foreign,
            Some(&mut mine_buf),
        );
        scan_records(
            &payload,
            resume,
            2,
            &mut mine,
            &mut foreign,
            Some(&mut mine_buf),
        );

        assert_eq!(4, mine);
        assert_eq!(
            payload, mine_buf,
            "the straddling record must appear exactly once, in order"
        );
    }

    /// Regression: a phased pump used to complete on the raw byte count, so records released
    /// late by an earlier phase filled the budget and the pump reported `Complete` before its
    /// own records had arrived. The collected buffer is what completion and integrity are now
    /// measured on, so it must contain this phase's records and nothing else.
    #[test]
    fn scanning_should_collect_only_this_phase_for_integrity() {
        let phase_one = tagged_payload(1, 3 * RECORD_SIZE);
        let phase_two = tagged_payload(2, 5 * RECORD_SIZE);
        let mut buf = phase_one.clone();
        buf.extend_from_slice(&phase_two);

        let (mut mine, mut foreign) = (0, 0);
        let mut mine_buf = Vec::new();
        scan_records(&buf, 0, 2, &mut mine, &mut foreign, Some(&mut mine_buf));

        assert_eq!(mine, 5);
        assert_eq!(foreign, 3);
        assert_eq!(
            phase_two, mine_buf,
            "the collected buffer must be phase 2's payload exactly, so its digest can be checked"
        );
    }

    /// Reads land on arbitrary boundaries, so a record routinely straddles two of them. Resuming
    /// from the returned position must count it exactly once — never dropped, never double-counted.
    #[test]
    fn a_record_split_across_two_reads_should_be_counted_once() {
        let buf = tagged_payload(1, 3 * RECORD_SIZE);
        let split = RECORD_SIZE + 5; // mid-record

        let (mut mine, mut foreign) = (0, 0);
        let resume = scan_records(&buf[..split], 0, 1, &mut mine, &mut foreign, None);
        assert_eq!(
            mine, 1,
            "only the first whole record fits in the first read"
        );

        scan_records(&buf, resume, 1, &mut mine, &mut foreign, None);
        assert_eq!(
            mine, 3,
            "the straddling record is picked up on the next scan"
        );
        assert_eq!(foreign, 0);
    }

    /// Loss removes whole segments mid-stream, which shifts everything after it. Attribution must
    /// survive that, since it is exactly the condition under test.
    #[test]
    fn attribution_should_survive_a_hole_in_the_stream() {
        let whole = tagged_payload(2, 10 * RECORD_SIZE);
        let mut lossy = whole[..3 * RECORD_SIZE].to_vec();
        lossy.extend_from_slice(&whole[5 * RECORD_SIZE + 7..]); // drop 2 records, misalign the rest

        let (mut mine, mut foreign) = (0, 0);
        scan_records(&lossy, 0, 2, &mut mine, &mut foreign, None);
        assert!(
            (7..=8).contains(&mine),
            "the surviving records must still be found after a hole, got {mine}"
        );
        assert_eq!(foreign, 0, "nothing here belongs to another phase");
    }

    // ── stopping rule ─────────────────────────────────────────────────────────

    const DEADLINE: Duration = Duration::from_secs(600);

    /// A return stream that has produced nothing has already failed; waiting out the remaining
    /// nine and a half minutes cannot change that. One run spent seven minutes measuring a session
    /// whose server side had already ended.
    #[test]
    fn a_stream_that_never_delivers_should_give_up_at_the_first_byte_budget() {
        assert_eq!(
            stop_reason(
                0,
                NO_FIRST_BYTE_TIMEOUT,
                NO_FIRST_BYTE_TIMEOUT,
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            Some(PumpOutcome::NeverStarted),
        );
        assert_eq!(
            stop_reason(
                0,
                NO_FIRST_BYTE_TIMEOUT - Duration::from_secs(1),
                NO_FIRST_BYTE_TIMEOUT - Duration::from_secs(1),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            None,
            "the budget must not expire early — a recovering stream may still be on its way"
        );
    }

    /// The idle rule needs an arrival to measure from, so before one exists it must not fire.
    /// Otherwise a stream that takes 11s to produce its first byte is cut off as idle.
    #[test]
    fn waiting_for_the_first_byte_should_not_be_cut_short_by_the_idle_rule() {
        assert_eq!(
            stop_reason(
                0,
                READ_IDLE_TIMEOUT * 2,
                READ_IDLE_TIMEOUT * 2,
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            None,
            "with nothing received the first-byte budget decides, not the idle timeout"
        );
    }

    /// Once bytes have arrived, silence means the tail is never coming (UDP loopback has no EOF).
    #[test]
    fn a_stream_that_goes_quiet_after_delivering_should_stop_as_idle() {
        assert_eq!(
            stop_reason(
                1_000,
                READ_IDLE_TIMEOUT,
                Duration::from_secs(60),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            Some(PumpOutcome::Idle),
        );
        assert_eq!(
            stop_reason(
                1_000,
                Duration::from_secs(1),
                Duration::from_secs(60),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            None,
            "a stream still delivering must be left alone"
        );
    }

    /// A stream that is still delivering when the deadline lands is a distinct outcome from one
    /// that stopped: it was making progress and simply ran out of time.
    #[test]
    fn a_stream_still_delivering_at_the_deadline_should_report_the_deadline() {
        assert_eq!(
            stop_reason(
                1_000,
                Duration::ZERO,
                DEADLINE,
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None
            ),
            Some(PumpOutcome::DeadlineExceeded),
        );
    }

    /// The two "nothing is serving this session" outcomes have to be separable from the two that
    /// mean "served, but badly" — that distinction is what a scenario branches on.
    #[test]
    fn only_the_no_service_outcomes_should_report_the_exit_stopped_serving() {
        assert!(PumpOutcome::NeverStarted.exit_stopped_serving());
        assert!(PumpOutcome::SessionClosed.exit_stopped_serving());
        assert!(!PumpOutcome::Idle.exit_stopped_serving());
        assert!(!PumpOutcome::DeadlineExceeded.exit_stopped_serving());
        assert!(!PumpOutcome::Complete.exit_stopped_serving());
        // The cap fires on a stream that was still arriving, so there was something on the other
        // end. Grouping it with the no-service outcomes would fail a merely slow return path
        // outright, instead of scoring what came back.
        assert!(!PumpOutcome::TailGraceExpired.exit_stopped_serving());
    }

    // ── tail cap ──────────────────────────────────────────────────────────────

    const TAIL_GRACE: Duration = Duration::from_secs(45);

    /// The case the idle rule structurally cannot catch: bytes keep arriving, so the stream is
    /// never quiet, but far too slowly to ever finish. One run trickled for 444 s to offer 60 s of
    /// data — and every second past the cap measured a draining backlog, not the return path.
    #[test]
    fn a_trickling_stream_should_be_cut_off_once_the_tail_grace_expires() {
        let still_arriving = Duration::ZERO;
        assert_eq!(
            stop_reason(
                1_000,
                still_arriving,
                Duration::from_secs(300),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                Some(TailWindow {
                    since_offer_complete: TAIL_GRACE,
                    grace: TAIL_GRACE,
                }),
            ),
            Some(PumpOutcome::TailGraceExpired),
            "a stream that is never idle and never done must still be bounded"
        );
    }

    /// The cap is measured from the *offer*, not from the start of the pump: while the writer is
    /// still handing over bytes there is nothing to have been waiting for.
    #[test]
    fn the_tail_cap_should_not_fire_while_the_offer_is_still_in_flight() {
        assert_eq!(
            stop_reason(
                1_000,
                Duration::ZERO,
                Duration::from_secs(300),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                None,
            ),
            None,
            "no tail window means the offer has not finished — the cap cannot have started"
        );
        assert_eq!(
            stop_reason(
                1_000,
                Duration::ZERO,
                Duration::from_secs(300),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                Some(TailWindow {
                    since_offer_complete: TAIL_GRACE - Duration::from_secs(1),
                    grace: TAIL_GRACE,
                }),
            ),
            None,
            "the grace must run its full length before the reader gives up"
        );
    }

    /// The cap decides *when* to stop asking, not what to conclude. A phase that received nothing
    /// at all was never served, and reporting that as a slow tail would hide it from the check
    /// that fails fast on a dead exit.
    #[test]
    fn the_tail_cap_should_still_report_a_phase_that_received_nothing_as_never_started() {
        assert_eq!(
            stop_reason(
                0,
                Duration::ZERO,
                Duration::from_secs(300),
                DEADLINE,
                READ_IDLE_TIMEOUT,
                Some(TailWindow {
                    since_offer_complete: TAIL_GRACE,
                    grace: TAIL_GRACE,
                }),
            ),
            Some(PumpOutcome::NeverStarted),
        );
    }

    /// The reason this metric exists: `mbps` is hostage to the tail, this is not.
    ///
    /// Both transfers below deliver the same 96 % of the payload, and the bulk of it at the same
    /// rate. They differ only in when the last stragglers land -- which is exactly the difference
    /// that made three identical cluster runs report 0.22, 0.22 and 0.29 MB/s.
    #[test]
    fn throughput_at_a_share_should_ignore_when_the_tail_lands() {
        // 9.6 MB in over ten seconds, then the last of it much later.
        let prompt = transfer(
            12.0,
            &[(5.0, 5_000_000), (10.0, 9_600_000), (11.0, 9_600_000)],
        );
        let dragging = transfer(
            85.0,
            &[(5.0, 5_000_000), (10.0, 9_600_000), (80.0, 9_600_000)],
        );

        let (a, b) = (
            prompt.throughput_at(0.95).expect("95% was reached"),
            dragging.throughput_at(0.95).expect("95% was reached"),
        );
        assert!(
            (a - b).abs() < 1e-9,
            "the tail must not move the figure: {a} vs {b}"
        );
        assert!(
            (a - 0.96).abs() < 1e-9,
            "9.6 MB by 10 s is 0.96 MB/s, got {a}"
        );
    }

    /// A share that never arrived has no throughput to report, and must not be confused with zero.
    #[test]
    fn throughput_at_a_share_that_never_arrived_should_be_none() {
        // The helper sizes `sent_bytes` from what arrived, so state the loss explicitly.
        let mut lossy = transfer(30.0, &[(1.0, 100), (2.0, 200)]);
        lossy.sent_bytes = 10_000;
        assert_eq!(lossy.throughput_at(0.95), None);

        // Nothing was offered, so there is no share of it to have arrived and no rate to report.
        // Answering `0` here would read as "delivered nothing at zero throughput" for a transfer
        // that was never asked to deliver anything.
        let mut empty = transfer(30.0, &[]);
        empty.sent_bytes = 0;
        assert_eq!(empty.throughput_at(0.95), None);
    }

    /// A pump that stops because the stream went quiet ends, by definition, with a trailing gap of
    /// exactly its idle budget. Counting that as a stall reports the detector's own threshold as
    /// the symptom — and since the budget is required to exceed the recovery deadline it is
    /// measuring, every idle-terminated run then fails a deadline check it structurally cannot
    /// pass. Seen on a run that delivered 97.8 % and recovered in 13.8 s, failed for a "30.1 s
    /// stall" that was the pump waiting to be sure the stream had ended.
    #[test]
    fn an_idle_terminated_pump_should_not_count_its_own_idle_budget_as_a_stall() {
        let mut quiet = transfer(32.0, &[(1.0, 1), (2.0, 2)]);
        quiet.outcome = PumpOutcome::Idle;

        assert!(
            (quiet.longest_stall() - 1.0).abs() < 1e-9,
            "the trailing silence is how the pump stopped, not a gap in delivery; got {}",
            quiet.longest_stall()
        );
    }

    /// The inverse, so the rule does not become "ignore trailing silence". When the pump was cut
    /// off while still expecting data, the silence up to that point is a real gap in delivery.
    #[test]
    fn a_pump_cut_off_while_still_waiting_should_count_the_trailing_silence() {
        for cut_short in [
            PumpOutcome::DeadlineExceeded,
            PumpOutcome::TailGraceExpired,
            PumpOutcome::SessionClosed,
        ] {
            let mut waiting = transfer(32.0, &[(1.0, 1), (2.0, 2)]);
            waiting.outcome = cut_short;

            assert!(
                (waiting.longest_stall() - 30.0).abs() < 1e-9,
                "{cut_short:?} stopped the pump while data was still expected, so the silence \
                 counts; got {}",
                waiting.longest_stall()
            );
        }
    }

    /// Nothing arrived at all: every statistic has to say so rather than divide by zero.
    #[test]
    fn a_stream_that_delivered_nothing_should_report_no_recovery_and_no_rate() {
        let dead = transfer(30.0, &[]);

        assert_eq!(dead.time_to_first_byte(), None);
        assert_eq!(dead.time_to_sustain(0.5, Duration::from_secs(3)), None);
        assert_eq!(dead.steady_state_mbps(Duration::from_secs(3)), 0.0);
        assert_eq!(dead.inter_arrival_quantile(0.5), None);
        assert!((dead.longest_stall() - 30.0).abs() < 1e-9);
    }
}
