//! gnosis_vpn-testenv's regression suite on the HOPR layer alone: gnosis_vpn-client's WireGuard
//! session ([`IntegrationEnv::open_vpn_session`]) carrying what the tunnel would, judged by testenv's
//! thresholds. Each test names the `tests/regression/test_tNN_*.py` it replaces.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};

use edgli::hopr_lib::exports::{
    network::types::prelude::{IpOrHost, SealedHost},
    transport::{SessionCapabilities, SessionCapability},
};
use hoprd_integration_test::{
    HoprSession, IntegrationEnv, SessionTarget,
    cluster::{NodeInfo, request_latency_profile, scrape_metrics},
    env::{VPN_MAIN_TIER, vpn_surb_tier},
    pump::{PumpOpts, Shape, Transfer, pace_for_rate_with_chunk, pump_halves, tagged_payload},
    relayers, step,
    udp_service::{self, Mode, PROBE_MAGIC, PUSH_FILL, UdpService, push_request},
};
use rand::RngExt as _;
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{self, ChecksumCapabilities, DeviceCapabilities, Medium},
    socket::{icmp, tcp},
    time::Instant as SmolInstant,
    wire::{HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf},
    sync::mpsc,
};

/// Link rules need the localcluster's latency relays, which only run with a latency profile.
const FLAT_LATENCY: &str = "default: \"1ms\"\n";
/// A full WireGuard data message at the client's 1420 B tunnel MTU.
const WG_DATAGRAM: usize = 1420 + 32;
/// testenv's probes send 1200 B of UDP payload; inside the tunnel that is 1200 + IP/UDP + WireGuard.
const PROBE_DATAGRAM: usize = 1200 + 28 + 32;
/// testenv's `SURB_RAMP_WAIT`: the idle after connect before anything is measured.
const RAMP_WAIT: Duration = Duration::from_secs(25);
/// How long the reader keeps going after the offer ends, for the last returns.
const READ_TAIL: Duration = Duration::from_secs(3);

const LOSS_MAX_PCT: f64 = 5.0;
const STALL_MAX_S: f64 = 5.0;
/// A probe that sent less than this share of its packets did not run; its loss figure is noise.
const SAMPLE_MIN_PCT: f64 = 80.0;
const CALL_BITS: u64 = 1_500_000;
const STREAM_BITS: u64 = 3_000_000;

/// A T04-style transfer: 10 MB offered as WireGuard datagrams at `BULK_MBIT`.
const BULK_BYTES: usize = 10_000_000;
const BULK_MBIT: f64 = 12.0;
/// testenv's `DOWN_MIN_MBIT`: catches a halving of the rate a healthy stack reaches.
const BULK_FLOOR_MBIT: f64 = 7.0;
const BULK_TIMEOUT: Duration = Duration::from_secs(240);
const ARRIVAL_MIN_PCT: f64 = 99.0;
/// On its ping SURB tier a fresh session carries ~0.65 Mbit/s of a 12 Mbit/s blast (7 % came
/// back locally). testenv gates a cold start only on collapse: something arrives, no long stall.
const COLD_ARRIVAL_MIN_PCT: f64 = 1.0;

// ─────────────────────────────────────────────────────────────────────────────
// The client, its far end and what one window of traffic did
// ─────────────────────────────────────────────────────────────────────────────

async fn vpn_env() -> anyhow::Result<IntegrationEnv> {
    request_latency_profile(FLAT_LATENCY);
    // A test that failed mid-impairment leaves its rules behind; the next one starts clean.
    IntegrationEnv::setup_with(async |cluster| {
        cluster.clear_link_rules()?;
        Ok(None)
    })
    .await
}

fn wg_capabilities() -> SessionCapabilities {
    SessionCapability::Segmentation | SessionCapability::NoDelay
}

async fn open(env: &IntegrationEnv, target: SessionTarget) -> anyhow::Result<HoprSession> {
    env.open_vpn_session(target, wg_capabilities(), vpn_surb_tier(VPN_MAIN_TIER))
        .await
}

async fn open_warm(
    env: &IntegrationEnv,
    far_end: &UdpService,
) -> anyhow::Result<(ReadHalf<HoprSession>, WriteHalf<HoprSession>)> {
    let session = open(env, far_end.target()).await?;
    tokio::time::sleep(RAMP_WAIT).await;
    Ok(tokio::io::split(session))
}

fn exit_node(env: &IntegrationEnv) -> anyhow::Result<NodeInfo> {
    let exit = env.dest_for(1)?;
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == exit)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("exit {exit} is not a cluster node"))
}

/// Echoes `bytes` of random data through an [`Mode::Echo`] far end, `chunk` bytes a datagram, at the
/// datagram rate of `BULK_MBIT` in full-size WireGuard datagrams: packets, not bytes, cost SURBs.
async fn bulk(
    rx: &mut ReadHalf<HoprSession>,
    tx: &mut WriteHalf<HoprSession>,
    bytes: usize,
    chunk: usize,
    min_arrival_pct: f64,
    label: &str,
) -> anyhow::Result<Transfer> {
    // Its own tag per transfer: a session reused after a lossy one still returns its late echoes.
    static NEXT_PHASE: AtomicU8 = AtomicU8::new(0);
    let phase = NEXT_PHASE.fetch_add(1, Ordering::Relaxed);
    let payload = tagged_payload(phase, bytes);
    let t = pump_halves(
        rx,
        tx,
        &payload,
        label,
        BULK_TIMEOUT,
        PumpOpts {
            pace: pace_for_rate_with_chunk(
                BULK_MBIT / 8.0 * chunk as f64 / WG_DATAGRAM as f64,
                chunk,
            ),
            chunk: Some(chunk),
            phase: Some(phase),
            ..PumpOpts::default()
        },
    )
    .await?;
    anyhow::ensure!(
        t.arrival_pct() >= min_arrival_pct
            && t.longest_stall() <= STALL_MAX_S
            && (t.attributed_bytes < t.sent_bytes || t.sha_ok),
        "{label}: {:.2} % came back (min {min_arrival_pct} %), stalled {:.1} s (max {STALL_MAX_S}), \
         outcome {:?}, sha_ok {}",
        t.arrival_pct(),
        t.longest_stall(),
        t.outcome,
        t.sha_ok
    );
    Ok(t)
}

/// Rate to 90 % of the payload in Mbit/s, so a few late stragglers do not move it.
fn mbit(t: &Transfer) -> f64 {
    t.throughput_at(0.9).unwrap_or(0.0) * 8.0
}

#[derive(Debug, Default)]
struct Window {
    duration_s: f64,
    probes_sent: u64,
    probes_back: u64,
    /// Echoes of a probe already counted, or of one from an earlier window on the session.
    probes_ignored: u64,
    probe_seqs_back: HashSet<u64>,
    rtts_ms: Vec<f64>,
    /// The writer was still blocked when the window ended, so it offered less than asked.
    send_deadline_missed: bool,
    push_expected: u64,
    fill_bytes: u64,
    first_arrival_s: Option<f64>,
    /// Longest silence after the first return, the tail to the end of the offer included.
    longest_gap_s: f64,
    /// Reads that were not exactly one datagram: `NoDelay` must keep their boundaries (T31).
    split_reads: u64,
    corrupt_bytes: u64,
}

/// Zero for a direction the window did not offer.
fn loss_pct(got: u64, of: u64) -> f64 {
    if of == 0 {
        return 0.0;
    }
    100.0 * (1.0 - got as f64 / of as f64).max(0.0)
}

impl Window {
    fn call_loss_pct(&self) -> f64 {
        loss_pct(self.probes_back, self.probes_sent)
    }

    fn download_loss_pct(&self) -> f64 {
        loss_pct(self.fill_bytes, self.push_expected)
    }

    fn sampled_pct(&self, call_bits: u64, datagram: usize) -> f64 {
        let expected = call_bits as f64 / 8.0 / datagram as f64 * self.duration_s;
        100.0 * self.probes_sent as f64 / expected.max(1.0)
    }

    /// Nearest-rank, as testenv computes it.
    fn rtt_ms(&self, q: f64) -> Option<f64> {
        let mut sorted = self.rtts_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let rank = ((q * sorted.len() as f64).ceil() as usize).max(1);
        sorted.get(rank - 1).copied()
    }

    fn summary(&self) -> String {
        format!(
            "call {}/{} back ({:.2} % lost, {} stale or duplicate), rtt p50 {:?} / p95 {:?} ms, \
             send deadline missed {}, download {}/{} B ({:.2} % lost), first return {:?} s, \
             longest gap {:.1} s, {} split reads, {} corrupt bytes",
            self.probes_back,
            self.probes_sent,
            self.call_loss_pct(),
            self.probes_ignored,
            self.rtt_ms(0.5).map(|v| v.round()),
            self.rtt_ms(0.95).map(|v| v.round()),
            self.send_deadline_missed,
            self.fill_bytes,
            self.push_expected,
            self.download_loss_pct(),
            self.first_arrival_s.map(|v| (v * 10.0).round() / 10.0),
            self.longest_gap_s,
            self.split_reads,
            self.corrupt_bytes,
        )
    }

    /// testenv's stream verdict: the stream ran, lost under `LOSS_MAX_PCT`, never went quiet for
    /// longer than `STALL_MAX_S`, and every datagram came back whole.
    fn check(&self, label: &str, loss: f64) -> anyhow::Result<()> {
        tracing::info!("{label}: {}", self.summary());
        anyhow::ensure!(
            loss < LOSS_MAX_PCT
                && !self.send_deadline_missed
                && self.first_arrival_s.is_some_and(|s| s <= STALL_MAX_S)
                && self.longest_gap_s <= STALL_MAX_S
                && self.split_reads == 0
                && self.corrupt_bytes == 0,
            "{label}: {loss:.2} % lost (max {LOSS_MAX_PCT}), gaps up to {STALL_MAX_S} s allowed; {}",
            self.summary()
        );
        Ok(())
    }
}

/// Offers a call and a download (either off at 0 bit/s) of `datagram`-sized packets for `duration`.
/// The download is a push the [`Mode::Echo`] far end starts on request.
async fn drive(
    rx: &mut ReadHalf<HoprSession>,
    tx: &mut WriteHalf<HoprSession>,
    duration: Duration,
    call_bits: u64,
    push_bits: u64,
    datagram: usize,
) -> anyhow::Result<Window> {
    let start = Instant::now();
    let end: tokio::time::Instant = (start + duration).into();
    let window_tag: u32 = rand::random();
    let mut window = Window {
        duration_s: duration.as_secs_f64(),
        ..Window::default()
    };
    if push_bits > 0 {
        let rate = (push_bits / (8 * datagram as u64)).max(1);
        window.push_expected = push_bits / 8 * duration.as_secs();
        tokio::time::timeout_at(
            end,
            tx.write_all(&push_request(datagram, rate, window.push_expected)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("the push request did not go out within the window"))??;
    }

    let send = async {
        let every = (call_bits > 0)
            .then(|| Duration::from_secs_f64(datagram as f64 * 8.0 / call_bits as f64));
        let mut probes = 0u64;
        let mut missed = false;
        if let Some(every) = every {
            let mut probe = vec![0u8; datagram];
            probe[..4].copy_from_slice(&PROBE_MAGIC);
            probe[20..24].copy_from_slice(&window_tag.to_le_bytes());
            let mut at = start;
            while at < start + duration {
                tokio::time::sleep_until(at.into()).await;
                probe[4..12].copy_from_slice(&probes.to_le_bytes());
                let sent_us = start.elapsed().as_micros() as u64;
                probe[12..20].copy_from_slice(&sent_us.to_le_bytes());
                let Ok(written) = tokio::time::timeout_at(end, tx.write_all(&probe)).await else {
                    missed = true;
                    break;
                };
                written?;
                probes += 1;
                at += every;
            }
        }
        missed |= tokio::time::timeout_at(end, tx.flush()).await.is_err();
        Ok::<_, std::io::Error>((probes, missed))
    };

    let recv = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending: Vec<u8> = Vec::new();
        let mut last: Option<f64> = None;
        let end = start + duration + READ_TAIL;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(read) = tokio::time::timeout(left, rx.read(&mut buf)).await else {
                break;
            };
            let n = read?;
            if n == 0 {
                tracing::warn!("the session closed under the window");
                break;
            }
            let at = start.elapsed().as_secs_f64();
            window.first_arrival_s.get_or_insert(at);
            if let Some(previous) = last {
                window.longest_gap_s = window.longest_gap_s.max(at - previous);
            }
            last = Some(at);
            window.split_reads += u64::from(n != datagram);
            pending.extend_from_slice(&buf[..n]);
            consume(&mut pending, &mut window, start, datagram, window_tag);
        }
        let quiet_since = last.unwrap_or(0.0);
        window.longest_gap_s = window.longest_gap_s.max(window.duration_s - quiet_since);
        Ok::<_, std::io::Error>(())
    };

    let (sent, read) = tokio::join!(send, recv);
    read?;
    (window.probes_sent, window.send_deadline_missed) = sent?;
    Ok(window)
}

/// Takes whole `datagram`-sized probes and runs of push fill off the front of `pending`; a byte that
/// is neither is corruption.
fn consume(
    pending: &mut Vec<u8>,
    window: &mut Window,
    start: Instant,
    datagram: usize,
    window_tag: u32,
) {
    loop {
        if pending.starts_with(&PROBE_MAGIC) {
            if pending.len() < datagram {
                return;
            }
            let field =
                |at: usize| u64::from_le_bytes(pending[at..at + 8].try_into().expect("8 bytes"));
            let (seq, sent_us) = (field(4), field(12));
            let tag = u32::from_le_bytes(pending[20..24].try_into().expect("4 bytes"));
            if tag == window_tag && window.probe_seqs_back.insert(seq) {
                let now_us = start.elapsed().as_micros() as u64;
                window.probes_back += 1;
                window
                    .rtts_ms
                    .push(now_us.saturating_sub(sent_us) as f64 / 1000.0);
            } else {
                window.probes_ignored += 1;
            }
            pending.drain(..datagram);
        } else if pending.first() == Some(&PUSH_FILL) {
            let run = pending.iter().take_while(|b| **b == PUSH_FILL).count();
            window.fill_bytes += run as u64;
            pending.drain(..run);
        } else if pending.is_empty() || PROBE_MAGIC.starts_with(pending) {
            return;
        } else {
            window.corrupt_bytes += 1;
            pending.remove(0);
        }
    }
}

/// Every `hopr_surb_balancer_*` series the exit holds: one per Session it shapes.
async fn shaped_sessions(exit: &NodeInfo) -> anyhow::Result<BTreeSet<String>> {
    Ok(scrape_metrics(exit)
        .await?
        .lines()
        .filter_map(|l| l.strip_prefix("hopr_surb_balancer_current_buffer_target{"))
        .filter_map(|l| l.split_once('}').map(|(labels, _)| labels.to_string()))
        .collect())
}

fn rss_kb() -> anyhow::Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no VmRSS in /proc/self/status"))
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP over the tunnel: smoltcp at both ends, one IP packet per WireGuard-sized datagram
// ─────────────────────────────────────────────────────────────────────────────

const TUNNEL_MTU: usize = 1420;
const WG_OVERHEAD: usize = WG_DATAGRAM - TUNNEL_MTU;
const CLIENT_IP: Ipv4Address = Ipv4Address::new(10, 128, 0, 2);
const SERVER_IP: Ipv4Address = Ipv4Address::new(10, 128, 0, 1);
const TCP_PORT: u16 = 5201;
/// Room for the bandwidth-delay product of 16 Mbit/s at a 2 s round trip.
const TCP_BUFFER: usize = 4 << 20;
/// Direction (1 = the client uploads), then the length, as 8 bytes little-endian.
const REQUEST_LEN: usize = 9;
const PING_EVERY: Duration = Duration::from_millis(100);
const PING_IDENT: u16 = 0x4850;
/// The longest smoltcp waits before it looks at its sockets again.
const STACK_TICK: Duration = Duration::from_millis(1);
/// hoprnet's default Poisson mixer holds a packet ~4.1 ms on average, at every node of the 1-hop
/// paths. Round trips that vary by less than that went around the mixers.
const JITTER_MIN_MS: f64 = 4.1;
/// Beyond this TCP's congestion control starts breaking down.
const JITTER_MAX_MS: f64 = 250.0;
const RTT_MEAN_MAX_MS: f64 = 1500.0;

/// IP packets in and out of one end of the tunnel, for smoltcp to poll.
#[derive(Default)]
struct Packets {
    inbound: VecDeque<Vec<u8>>,
    outbound: Vec<Vec<u8>>,
}

struct Inbound(Vec<u8>);
struct Outbound<'a>(&'a mut Vec<Vec<u8>>);

impl phy::RxToken for Inbound {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.0.get(WG_OVERHEAD..).unwrap_or_default())
    }
}

impl phy::TxToken for Outbound<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut datagram = vec![0u8; WG_OVERHEAD + len];
        let result = f(&mut datagram[WG_OVERHEAD..]);
        self.0.push(datagram);
        result
    }
}

impl phy::Device for Packets {
    type RxToken<'a> = Inbound;
    type TxToken<'a> = Outbound<'a>;

    fn receive(&mut self, _: SmolInstant) -> Option<(Inbound, Outbound<'_>)> {
        let datagram = self.inbound.pop_front()?;
        Some((Inbound(datagram), Outbound(&mut self.outbound)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<Outbound<'_>> {
        Some(Outbound(&mut self.outbound))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = TUNNEL_MTU;
        caps
    }
}

/// Runs one end's IP stack on the datagrams of `inbound` and `outbound` until `app` is done.
async fn run_stack(
    ip: Ipv4Address,
    inbound: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    outbound: &mpsc::UnboundedSender<Vec<u8>>,
    mut app: impl FnMut(&mut Interface, &mut SocketSet<'static>) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut packets = Packets::default();
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = rand::random();
    let mut iface = Interface::new(config, &mut packets, SmolInstant::now());
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(ip.into(), 24))
            .expect("room for one address");
    });
    let mut sockets = SocketSet::new(vec![]);
    loop {
        iface.poll(SmolInstant::now(), &mut packets, &mut sockets);
        let done = app(&mut iface, &mut sockets)?;
        iface.poll(SmolInstant::now(), &mut packets, &mut sockets);
        for datagram in packets.outbound.drain(..) {
            let _ = outbound.send(datagram);
        }
        if done {
            return Ok(());
        }
        let wait = iface
            .poll_delay(SmolInstant::now(), &sockets)
            .map_or(STACK_TICK, |d| Duration::from(d).min(STACK_TICK));
        tokio::select! {
            datagram = inbound.recv() => {
                packets.inbound.push_back(datagram.ok_or_else(|| anyhow::anyhow!("the tunnel closed"))?);
            }
            _ = tokio::time::sleep(wait) => {}
        }
        while let Ok(datagram) = inbound.try_recv() {
            packets.inbound.push_back(datagram);
        }
    }
}

fn tcp_socket() -> tcp::Socket<'static> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
        tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
    );
    socket.set_congestion_control(tcp::CongestionControl::Cubic);
    socket
}

fn pattern_at(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn fill_pattern(buf: &mut [u8], from: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern_at(from + i as u64);
    }
}

fn is_pattern(data: &[u8], from: u64) -> bool {
    data.iter()
        .enumerate()
        .all(|(i, b)| *b == pattern_at(from + i as u64))
}

/// The client's end of the tunnel on a session. Dropping it drops the session.
struct Link {
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    reader: tokio::task::AbortHandle,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn session_link(session: HoprSession) -> Link {
    let (mut rx, mut tx) = tokio::io::split(session);
    let (to_stack, inbound) = mpsc::unbounded_channel();
    let (outbound, mut from_stack) = mpsc::unbounded_channel::<Vec<u8>>();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n @ 1..) = rx.read(&mut buf).await {
            if to_stack.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    })
    .abort_handle();
    tokio::spawn(async move {
        while let Some(datagram) = from_stack.recv().await {
            if tx.write_all(&datagram).await.is_err() {
                break;
            }
        }
    });
    Link {
        inbound,
        outbound,
        reader,
    }
}

/// The far end: a UDP socket the exit forwards the session to, and a TCP server behind it.
struct TcpServer {
    addr: SocketAddr,
    task: tokio::task::AbortHandle,
}

impl Drop for TcpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TcpServer {
    fn target(&self) -> SessionTarget {
        SessionTarget::UdpStream(SealedHost::Plain(IpOrHost::Ip(self.addr)))
    }
}

async fn spawn_tcp_server() -> anyhow::Result<TcpServer> {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let addr = socket.local_addr()?;
    let task = tokio::spawn(async move {
        let (to_stack, mut inbound) = mpsc::unbounded_channel();
        let (outbound, mut from_stack) = mpsc::unbounded_channel::<Vec<u8>>();
        let io = async {
            let mut buf = vec![0u8; 4096];
            let mut peer = None;
            loop {
                tokio::select! {
                    got = socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = got else { break };
                        peer = Some(from);
                        let _ = to_stack.send(buf[..n].to_vec());
                    }
                    Some(datagram) = from_stack.recv() => {
                        if let Some(peer) = peer {
                            let _ = socket.send_to(&datagram, peer).await;
                        }
                    }
                }
            }
        };
        let mut conns = Vec::new();
        let stack = run_stack(SERVER_IP, &mut inbound, &outbound, |_, sockets| {
            serve(sockets, &mut conns);
            Ok(false)
        });
        tokio::select! {
            _ = io => {}
            Err(error) = stack => tracing::warn!(%error, "TCP server stopped"),
        }
    })
    .abort_handle();
    Ok(TcpServer { addr, task })
}

/// One request per connection: the server sinks an upload, or sends a download, then closes.
fn serve(sockets: &mut SocketSet<'static>, conns: &mut Vec<(SocketHandle, Vec<u8>, u64)>) {
    conns.retain(|(handle, ..)| {
        let open = sockets.get::<tcp::Socket>(*handle).state() != tcp::State::Closed;
        if !open {
            sockets.remove(*handle);
        }
        open
    });
    let listening = conns
        .iter()
        .any(|(handle, ..)| sockets.get::<tcp::Socket>(*handle).state() == tcp::State::Listen);
    if !listening {
        let mut socket = tcp_socket();
        socket.listen(TCP_PORT).expect("a fresh socket listens");
        conns.push((sockets.add(socket), Vec::new(), 0));
    }
    for (handle, request, moved) in conns.iter_mut() {
        let socket = sockets.get_mut::<tcp::Socket>(*handle);
        while request.len() < REQUEST_LEN && socket.can_recv() {
            let want = REQUEST_LEN - request.len();
            let _ = socket.recv(|data| {
                let n = data.len().min(want);
                request.extend_from_slice(&data[..n]);
                (n, ())
            });
        }
        let Ok(length): Result<[u8; 8], _> =
            request.get(1..REQUEST_LEN).unwrap_or_default().try_into()
        else {
            continue;
        };
        let bytes = u64::from_le_bytes(length);
        if request[0] == 1 {
            while socket.can_recv() {
                let Ok((n, intact)) =
                    socket.recv(|data| (data.len(), (data.len(), is_pattern(data, *moved))))
                else {
                    break;
                };
                if !intact {
                    tracing::warn!(offset = *moved, "TCP server: the upload arrived corrupt");
                    socket.abort();
                    break;
                }
                *moved += n as u64;
            }
        } else {
            while *moved < bytes && socket.can_send() {
                let Ok(n) = socket.send(|buf| {
                    let n = buf.len().min((bytes - *moved) as usize);
                    fill_pattern(&mut buf[..n], *moved);
                    (n, n)
                }) else {
                    break;
                };
                *moved += n as u64;
            }
        }
        if *moved >= bytes {
            socket.close();
        }
    }
}

/// One TCP transfer and the pings next to it.
#[derive(Debug, Default)]
struct TcpRun {
    label: String,
    bytes: u64,
    seconds: f64,
    /// Longest time without progress, from the connect on: acknowledged bytes up, received down.
    longest_gap_s: f64,
    rtts_ms: Vec<f64>,
}

impl TcpRun {
    fn mbit(&self) -> f64 {
        self.bytes as f64 * 8.0 / 1e6 / self.seconds
    }

    fn rtt_mean_ms(&self) -> f64 {
        self.rtts_ms.iter().sum::<f64>() / self.rtts_ms.len().max(1) as f64
    }

    /// Mean change between consecutive round trips.
    fn jitter_ms(&self) -> f64 {
        let deltas: Vec<f64> = self
            .rtts_ms
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .collect();
        deltas.iter().sum::<f64>() / deltas.len().max(1) as f64
    }

    fn summary(&self) -> String {
        format!(
            "{} B in {:.1} s ({:.1} Mbit/s), longest gap {:.1} s, {} pings: rtt mean {:.0} ms, \
             jitter {:.1} ms",
            self.bytes,
            self.seconds,
            self.mbit(),
            self.longest_gap_s,
            self.rtts_ms.len(),
            self.rtt_mean_ms(),
            self.jitter_ms(),
        )
    }

    fn check(&self, min_mbit: f64) -> anyhow::Result<()> {
        tracing::info!("{}: {}", self.label, self.summary());
        anyhow::ensure!(
            self.longest_gap_s <= STALL_MAX_S
                && self.mbit() >= min_mbit
                && self.rtts_ms.len() >= 2
                && self.rtt_mean_ms() <= RTT_MEAN_MAX_MS
                && (JITTER_MIN_MS..=JITTER_MAX_MS).contains(&self.jitter_ms()),
            "{}: {} (min {min_mbit} Mbit/s, gap max {STALL_MAX_S} s, rtt mean max \
             {RTT_MEAN_MAX_MS} ms, jitter {JITTER_MIN_MS}-{JITTER_MAX_MS} ms)",
            self.label,
            self.summary()
        );
        Ok(())
    }
}

/// Moves `bytes` over a fresh TCP connection through `link`, up or down, and pings the far end every
/// `PING_EVERY` meanwhile. Uploads are held to `rate_mbit`; the timeout allows for it, or the bulk floor.
async fn tcp_transfer(
    link: &mut Link,
    upload: bool,
    bytes: u64,
    rate_mbit: Option<f64>,
    label: &str,
) -> anyhow::Result<TcpRun> {
    let floor_mbit = rate_mbit.unwrap_or(BULK_FLOOR_MBIT);
    let timeout = Duration::from_secs_f64(60.0 + 2.0 * bytes as f64 * 8.0 / 1e6 / floor_mbit);
    let request = [&[u8::from(upload)][..], &bytes.to_le_bytes()].concat();
    let local_port: u16 = rand::random_range(49152..65535);
    let start = Instant::now();
    let mut run = TcpRun {
        label: label.to_string(),
        bytes,
        ..TcpRun::default()
    };
    let (mut conn, mut pinger) = (None, None);
    let (mut requested, mut moved, mut progress) = (0, 0u64, 0u64);
    let mut last_progress = start;
    let mut pings = HashMap::new();
    let (mut seq, mut next_ping) = (0u16, start);
    run_stack(
        CLIENT_IP,
        &mut link.inbound,
        &link.outbound,
        |iface, sockets| {
            let now = Instant::now();
            anyhow::ensure!(
                now - start < timeout,
                "{label}: {progress}/{bytes} B after {timeout:?}"
            );
            let ping_handle = match pinger {
                Some(handle) => handle,
                None => {
                    let buffer = || {
                        icmp::PacketBuffer::new(
                            vec![icmp::PacketMetadata::EMPTY; 64],
                            vec![0; 8192],
                        )
                    };
                    let mut socket = icmp::Socket::new(buffer(), buffer());
                    socket.bind(icmp::Endpoint::Ident(PING_IDENT))?;
                    *pinger.insert(sockets.add(socket))
                }
            };
            let ping = sockets.get_mut::<icmp::Socket>(ping_handle);
            if now >= next_ping && ping.can_send() {
                let echo = Icmpv4Repr::EchoRequest {
                    ident: PING_IDENT,
                    seq_no: seq,
                    data: &[0u8; 56],
                };
                let buf = ping.send(echo.buffer_len(), IpAddress::Ipv4(SERVER_IP))?;
                echo.emit(
                    &mut Icmpv4Packet::new_unchecked(buf),
                    &ChecksumCapabilities::default(),
                );
                pings.insert(seq, now);
                seq = seq.wrapping_add(1);
                next_ping = now + PING_EVERY;
            }
            while let Ok((payload, _)) = ping.recv() {
                let reply = Icmpv4Packet::new_checked(payload)
                    .and_then(|p| Icmpv4Repr::parse(&p, &ChecksumCapabilities::default()));
                if let Ok(Icmpv4Repr::EchoReply { seq_no, .. }) = reply
                    && let Some(sent) = pings.remove(&seq_no)
                {
                    run.rtts_ms.push((now - sent).as_secs_f64() * 1000.0);
                }
            }

            let tcp_handle = match conn {
                Some(handle) => handle,
                None => {
                    let mut socket = tcp_socket();
                    socket.connect(
                        iface.context(),
                        (IpAddress::Ipv4(SERVER_IP), TCP_PORT),
                        local_port,
                    )?;
                    *conn.insert(sockets.add(socket))
                }
            };
            let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            if requested < REQUEST_LEN && socket.can_send() {
                requested += socket.send_slice(&request[requested..])?;
            }
            let done = if upload {
                let allowed = rate_mbit
                    .map_or(bytes, |mbit| {
                        (mbit * 1e6 / 8.0 * (now - start).as_secs_f64()) as u64
                    })
                    .min(bytes);
                while requested == REQUEST_LEN && moved < allowed && socket.can_send() {
                    let n = socket.send(|buf| {
                        let n = buf.len().min((allowed - moved) as usize);
                        fill_pattern(&mut buf[..n], moved);
                        (n, n)
                    })?;
                    moved += n as u64;
                }
                (requested as u64 + moved)
                    .saturating_sub(socket.send_queue() as u64)
                    .saturating_sub(REQUEST_LEN as u64)
            } else {
                while socket.can_recv() {
                    let (n, intact) =
                        socket.recv(|data| (data.len(), (data.len(), is_pattern(data, moved))))?;
                    anyhow::ensure!(intact, "{label}: corrupt bytes from offset {moved}");
                    moved += n as u64;
                }
                moved
            };
            if done > progress {
                run.longest_gap_s = run.longest_gap_s.max((now - last_progress).as_secs_f64());
                (progress, last_progress) = (done, now);
            }
            if run.seconds == 0.0 {
                anyhow::ensure!(
                    socket.is_open(),
                    "{label}: the connection reset at {progress}/{bytes} B"
                );
                if progress == bytes {
                    run.seconds = (now - start).as_secs_f64();
                    socket.close();
                }
                return Ok(false);
            }
            Ok(!socket.is_open())
        },
    )
    .await?;
    Ok(run)
}

// ─────────────────────────────────────────────────────────────────────────────
// Throughput
// ─────────────────────────────────────────────────────────────────────────────

/// T04-fixed-throughput, T05-loaded-latency and T06's upload and download arms, over TCP as the
/// tunnel mostly carries them: each size completes both ways without a stall and at the floor, while
/// pings next to it keep their round trip and jitter bounded.
async fn check_tcp_transfers(megabytes: &[u64]) -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let server = spawn_tcp_server().await?;
    let mut link = session_link(open(&env, server.target()).await?);
    tokio::time::sleep(RAMP_WAIT).await;
    for &size in megabytes {
        for upload in [false, true] {
            let label = format!("{size} MB {}", if upload { "up" } else { "down" });
            step(&label, async {
                tcp_transfer(&mut link, upload, size * 1_000_000, None, &label)
                    .await?
                    .check(BULK_FLOOR_MBIT)
            })
            .await?;
        }
    }
    Ok(())
}

/// The PR tier's size. No 5 MB step: it ends inside the session's ramp and swung 6.5-14.1 Mbit/s
/// around the 7 Mbit/s floor; 20 MB held ≥ 8.6.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn short_tcp_transfers_should_keep_their_rate_and_latency() -> anyhow::Result<()> {
    check_tcp_transfers(&[20]).await
}

/// The nightly tier's sizes.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn long_tcp_transfers_should_keep_their_rate_and_latency() -> anyhow::Result<()> {
    check_tcp_transfers(&[200, 500]).await
}

/// T07-cold-start and T15-warmup-knee: a fresh session carries a transfer at any idle after connect,
/// and reaches 0.8 of its best rate by 30 s. The knee sat at ~240 s on the broken 0.96.1 ramp.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_fresh_session_should_reach_full_rate_within_its_ramp() -> anyhow::Result<()> {
    const KNEE_FRACTION: f64 = 0.8;
    const KNEE_MAX_S: u64 = 30;

    let env = step("setup", vpn_env()).await?;
    let mut rates = Vec::new();
    for idle in [0, 5, 15, 30, 60] {
        let rate = step(&format!("{idle} s after connect"), async {
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = tokio::io::split(open(&env, far_end.target()).await?);
            tokio::time::sleep(Duration::from_secs(idle)).await;
            let floor = if Duration::from_secs(idle) < RAMP_WAIT {
                COLD_ARRIVAL_MIN_PCT
            } else {
                ARRIVAL_MIN_PCT
            };
            let t = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                floor,
                &format!("idle {idle} s"),
            )
            .await?;
            Ok(mbit(&t))
        })
        .await?;
        rates.push((idle, rate));
    }
    let best = rates.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let knee = rates.iter().find(|(_, r)| *r >= KNEE_FRACTION * best);
    tracing::info!(?rates, ?knee, "warm-up knee");
    anyhow::ensure!(
        knee.is_some_and(|(idle, _)| *idle <= KNEE_MAX_S),
        "no idle up to {KNEE_MAX_S} s reached {KNEE_FRACTION} of the best rate: {rates:?} (Mbit/s)"
    );
    Ok(())
}

/// T12-balancer-sweep: the client's main SURB upstream, swept over a known cliff (96 Mb/s collapsed
/// under a discard storm while 48 held): every cell completes and the worst warm rate holds at
/// least 0.3× the best.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn every_surb_upstream_should_carry_downloads() -> anyhow::Result<()> {
    const WORST_TO_BEST: f64 = 0.3;

    let env = step("setup", vpn_env()).await?;
    let mut warm = Vec::new();
    for upstream_mbit in [12, 16, 48, 96] {
        let rate = step(&format!("{upstream_mbit} Mb/s"), async {
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let main = vpn_surb_tier((VPN_MAIN_TIER.0, upstream_mbit * 1_000_000));
            let session = env
                .open_vpn_session(far_end.target(), wg_capabilities(), main)
                .await?;
            let (mut rx, mut tx) = tokio::io::split(session);
            bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                COLD_ARRIVAL_MIN_PCT,
                "cold",
            )
            .await?;
            tokio::time::sleep(RAMP_WAIT).await;
            let t = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                "warm",
            )
            .await?;
            Ok(mbit(&t))
        })
        .await?;
        warm.push((upstream_mbit, rate));
    }
    let best = warm.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let worst = warm.iter().map(|(_, r)| *r).fold(f64::INFINITY, f64::min);
    tracing::info!(?warm, "warm download per SURB upstream (Mbit/s)");
    anyhow::ensure!(
        best > 0.0 && worst >= WORST_TO_BEST * best,
        "warm downloads spread from {worst:.1} to {best:.1} Mbit/s: {warm:?}"
    );
    Ok(())
}

/// T18-capacity-ceiling and T06's after-bulk arm: on a session that has just carried three transfers
/// (hoprd 4.1.3 then showed up to 71 % loss), the download stream holds testenv's verdict up to the
/// knee and under 5 % loss beyond it. The fleet's exit topped out near 2000 packets/s.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_download_after_transfers_should_hold_up_to_the_capacity_knee() -> anyhow::Result<()> {
    const STEP: Duration = Duration::from_secs(30);
    const KNEE_MIN_MBIT: u64 = 8;

    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    step("transfers", async {
        for rep in 1..=3 {
            bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                &format!("transfer {rep}"),
            )
            .await?;
        }
        Ok(())
    })
    .await?;
    let mut knee = 0;
    for rung_mbit in [1, 2, 4, 8, 12, 16] {
        let label = format!("{rung_mbit} Mbit/s");
        let loss = step(&label, async {
            let w = drive(
                &mut rx,
                &mut tx,
                STEP,
                0,
                rung_mbit * 1_000_000,
                PROBE_DATAGRAM,
            )
            .await?;
            if rung_mbit <= KNEE_MIN_MBIT {
                w.check(&label, w.download_loss_pct())?;
            }
            tracing::info!("{label}: {}", w.summary());
            Ok(w.download_loss_pct())
        })
        .await?;
        if loss >= LOSS_MAX_PCT {
            break;
        }
        knee = rung_mbit;
    }
    anyhow::ensure!(
        knee >= KNEE_MIN_MBIT,
        "the download held under {LOSS_MAX_PCT} % loss only up to {knee} Mbit/s (min {KNEE_MIN_MBIT})"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration, contention and faults
// ─────────────────────────────────────────────────────────────────────────────

/// T11-capability-matrix: the exit shapes a session (runs a SURB balancer for it) exactly when the
/// client leaves rate control on, and every capability set carries a call.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn the_exit_should_shape_exactly_the_rate_controlled_sessions() -> anyhow::Result<()> {
    use SessionCapability::{NoDelay, NoRateControl, Segmentation};
    const POLLS: usize = 10;

    let env = step("setup", vpn_env()).await?;
    let exit = exit_node(&env)?;
    let cells: [(&str, SessionCapabilities, bool); 3] = [
        ("segmentation+no_delay", Segmentation | NoDelay, true),
        ("segmentation", Segmentation.into(), true),
        (
            "+no_rate_control",
            Segmentation | NoDelay | NoRateControl,
            false,
        ),
    ];
    for (name, capabilities, shaped) in cells {
        step(name, async {
            let before = shaped_sessions(&exit).await?;
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let session = env
                .open_vpn_session(far_end.target(), capabilities, vpn_surb_tier(VPN_MAIN_TIER))
                .await?;
            let (mut rx, mut tx) = tokio::io::split(session);
            let w = drive(
                &mut rx,
                &mut tx,
                Duration::from_secs(30),
                CALL_BITS,
                0,
                PROBE_DATAGRAM,
            )
            .await?;
            tracing::info!("{name}: {}", w.summary());
            anyhow::ensure!(
                w.call_loss_pct() < LOSS_MAX_PCT && w.corrupt_bytes == 0,
                "{name}: {}",
                w.summary()
            );
            let mut rose = false;
            for _ in 0..POLLS {
                rose = shaped_sessions(&exit)
                    .await?
                    .difference(&before)
                    .next()
                    .is_some();
                if rose {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            anyhow::ensure!(
                rose == shaped,
                "{name}: the exit {} a SURB balancer for the session",
                if rose { "runs" } else { "runs no" }
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T19-background-load: a 10 or 100 kB request every 2 s on another session to the same exit must
/// not halve a transfer. On the fleet a 10 kB trickle cut a download from 4.4-6.6 to 1.4-2.0 Mbit/s.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_trickle_on_another_session_should_not_starve_a_transfer() -> anyhow::Result<()> {
    const STEP_FRACTION: f64 = 0.5;
    const TRICKLE_EVERY: Duration = Duration::from_secs(2);
    const TRICKLE_REQUESTS: u64 = 10;

    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let trickle_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    let (mut trx, mut ttx) = open_warm(&env, &trickle_end).await?;
    let control = step("control", async {
        Ok(mbit(
            &bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                "control",
            )
            .await?,
        ))
    })
    .await?;
    for trickle in [10_000, 100_000] {
        step(&format!("{} kB trickle", trickle / 1000), async {
            let mut requests = vec![0u8; trickle * TRICKLE_REQUESTS as usize];
            rand::rng().fill(&mut requests[..]);
            let trickling = pump_halves(
                &mut trx,
                &mut ttx,
                &requests,
                "trickle",
                BULK_TIMEOUT,
                PumpOpts {
                    pace: pace_for_rate_with_chunk(BULK_MBIT / 8.0, WG_DATAGRAM),
                    chunk: Some(WG_DATAGRAM),
                    shape: Some(Shape::Burst {
                        on: trickle,
                        off: TRICKLE_EVERY,
                    }),
                    ..PumpOpts::default()
                },
            );
            let reached_before = trickle_end.received();
            let transfer = async {
                let started = Instant::now();
                let t = bulk(
                    &mut rx,
                    &mut tx,
                    BULK_BYTES,
                    WG_DATAGRAM,
                    ARRIVAL_MIN_PCT,
                    "under trickle",
                )
                .await?;
                anyhow::Ok((t, started.elapsed(), trickle_end.received()))
            };
            let (trickled, transferred) = tokio::join!(trickling, transfer);
            trickled?;
            let (t, took, reached_after) = transferred?;
            // Requests start at 0 s and then every `TRICKLE_EVERY` plus the burst itself.
            let requests_due = (took.as_secs_f64() / TRICKLE_EVERY.as_secs_f64()) as u64;
            let reached = reached_after - reached_before;
            anyhow::ensure!(
                reached >= requests_due.min(TRICKLE_REQUESTS) * trickle as u64,
                "the trickle reached its far end with {reached} B in the {took:?} transfer, \
                 {requests_due} requests of {trickle} B were due"
            );
            let rate = mbit(&t);
            anyhow::ensure!(
                rate >= STEP_FRACTION * control,
                "{rate:.1} Mbit/s under the trickle against {control:.1} without"
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T22-concurrent-clients: 1 and 10 sessions on one exit download over TCP at once; all complete,
/// ten carry no less than 75 % of what one did, and at least 8 Mbit/s together.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn concurrent_sessions_should_share_the_exit() -> anyhow::Result<()> {
    const TOLERANCE: f64 = 0.25;
    const AGGREGATE_MIN_MBIT: f64 = 8.0;

    let env = step("setup", vpn_env()).await?;
    let mut aggregates: Vec<(usize, f64)> = Vec::new();
    for sessions in [1, 10] {
        let aggregate = step(&format!("{sessions} sessions"), async {
            let mut ends = Vec::new();
            for _ in 0..sessions {
                let server = spawn_tcp_server().await?;
                let link = session_link(open(&env, server.target()).await?);
                ends.push((server, link));
            }
            tokio::time::sleep(RAMP_WAIT).await;
            let runs = futures::future::try_join_all(ends.iter_mut().enumerate().map(
                |(i, (_, link))| async move {
                    let run = tcp_transfer(
                        link,
                        false,
                        BULK_BYTES as u64,
                        Some(AGGREGATE_MIN_MBIT / sessions as f64),
                        &format!("session {i}"),
                    )
                    .await?;
                    tracing::info!("session {i}: {}", run.summary());
                    anyhow::Ok(run)
                },
            ))
            .await?;
            let bytes: u64 = runs.iter().map(|r| r.bytes).sum();
            let busy_s = runs.iter().map(|r| r.seconds).fold(0.0, f64::max);
            Ok(bytes as f64 * 8.0 / 1e6 / busy_s)
        })
        .await?;
        tracing::info!(sessions, aggregate, "aggregate Mbit/s");
        if let Some((lower, rate)) = aggregates
            .iter()
            .find(|(_, rate)| aggregate < (1.0 - TOLERANCE) * rate)
        {
            anyhow::bail!(
                "{sessions} sessions carried {aggregate:.1} Mbit/s, {lower} carried {rate:.1}"
            );
        }
        aggregates.push((sessions, aggregate));
    }
    let top = aggregates.last().map_or(0.0, |(_, rate)| *rate);
    anyhow::ensure!(
        top >= AGGREGATE_MIN_MBIT,
        "the top rung carried {top:.1} Mbit/s (min {AGGREGATE_MIN_MBIT})"
    );
    Ok(())
}

/// T09-impairment-ladder: return relays of equal 25 or 50 ms carry a session, and so do a near and a
/// far one together. On the first self-hosted exit that RTT mismatch was the defect: a 20-340 ms
/// relay mix gave 0.2 Mbit/s where one near relay gave 4.6-6.3.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn return_relays_of_unequal_latency_should_carry_the_session() -> anyhow::Result<()> {
    const STREAM_FOR: Duration = Duration::from_secs(60);
    /// Time for the entry's probes to see the new latencies before the session opens.
    const LEARN: Duration = Duration::from_secs(15);
    /// A stray re-plan or probe is not "carrying" the stream.
    const FAR_RELAY_MIN_SHARE: f64 = 0.05;
    const CELLS: [(&str, u64, u64); 4] = [
        ("equal-25", 25, 25),
        ("equal-50", 50, 50),
        ("gap-50", 50, 0),
        ("far-100", 0, 100),
    ];

    let env = step("setup", vpn_env()).await?;
    let cluster = env.cluster()?;
    let exit = env.dest_for(1)?;
    let relays = env.relayer_candidates(exit)?;
    anyhow::ensure!(
        relays.len() >= 2,
        "needs two return relays, have {}",
        relays.len()
    );
    for (name, first_ms, second_ms) in CELLS {
        let cell = step(name, async {
            cluster.clear_link_rules()?;
            for (relay, ms) in [(&relays[0], first_ms), (&relays[1], second_ms)] {
                if ms > 0 {
                    cluster.delay_link(exit, relay.address, Duration::from_millis(ms))?;
                }
            }
            tokio::time::sleep(LEARN).await;
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
            let forwarded_before = relayers::sample(&relays).await;
            let w = drive(
                &mut rx,
                &mut tx,
                STREAM_FOR,
                0,
                STREAM_BITS,
                PROBE_DATAGRAM,
            )
            .await?;
            let spread = relayers::spread(&forwarded_before, &relayers::sample(&relays).await);
            tracing::info!("{name}: relayers {}", spread.summary());
            w.check(name, w.download_loss_pct())?;
            // Without the far relay in the mix there is no RTT mismatch for the cell to catch.
            if first_ms != second_ms {
                let far = if first_ms > second_ms {
                    &relays[0]
                } else {
                    &relays[1]
                };
                anyhow::ensure!(
                    spread
                        .active_relayers(FAR_RELAY_MIN_SHARE)
                        .iter()
                        .any(|(address, _)| *address == far.address),
                    "{name}: the far relay {} carried under {FAR_RELAY_MIN_SHARE} of the stream: {}",
                    far.address,
                    spread.summary()
                );
            }
            bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                &format!("{name} transfer"),
            )
            .await?;
            Ok(())
        })
        .await;
        cluster.clear_link_rules()?;
        cell?;
    }
    Ok(())
}

/// T10-forced-reconnect: the client drops its session while the far end keeps streaming into it and
/// opens a fresh one at once. On 2026-09-14 every such fresh session died 3-7 s after it was ready.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_session_reopened_under_inbound_traffic_should_carry_it() -> anyhow::Result<()> {
    const BEFORE_DROP: Duration = Duration::from_secs(15);
    const AFTER: Duration = Duration::from_secs(60);
    const FIRST_PACKET_MAX_S: f64 = 10.0;

    let env = step("setup", vpn_env()).await?;
    let old_far_end = udp_service::spawn(Mode::Echo).await?;
    step("drop under a stream", async {
        let (rx, mut tx) = open_warm(&env, &old_far_end).await?;
        let rate = STREAM_BITS / (8 * PROBE_DATAGRAM as u64);
        let total = STREAM_BITS / 8 * (BEFORE_DROP + AFTER).as_secs();
        tx.write_all(&push_request(PROBE_DATAGRAM, rate, total))
            .await?;
        tokio::time::sleep(BEFORE_DROP).await;
        drop(rx.unsplit(tx));
        Ok(())
    })
    .await?;
    step("reopen", async {
        let far_end = udp_service::spawn(Mode::Echo).await?;
        let (mut rx, mut tx) = tokio::io::split(open(&env, far_end.target()).await?);
        let w = drive(&mut rx, &mut tx, AFTER, 0, STREAM_BITS, PROBE_DATAGRAM).await?;
        tracing::info!("reopened: {}", w.summary());
        // The startup allowance is charged to the loss budget: that much of the push may miss.
        let loss_max = LOSS_MAX_PCT + 100.0 * FIRST_PACKET_MAX_S / AFTER.as_secs_f64();
        anyhow::ensure!(
            w.first_arrival_s.is_some_and(|s| s <= FIRST_PACKET_MAX_S)
                && w.longest_gap_s <= STALL_MAX_S
                && w.download_loss_pct() < loss_max
                && w.corrupt_bytes == 0,
            "the reopened session (loss max {loss_max:.1} %): {}",
            w.summary()
        );
        Ok(())
    })
    .await
}

/// T23-sustained-soak, T24-sustained-upload and T06's idle arm: for ten minutes a call, a transfer a
/// minute and a 3 Mbit/s TCP upload hold with bounded memory, and a session idle 2 min on keep-alives
/// carries a call. testenv's upload lost its return path at ~405 s; a 75 s idle let SURBs expire.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_long_session_should_hold_its_loss_and_memory() -> anyhow::Result<()> {
    const SOAK: Duration = Duration::from_secs(600);
    const TRANSFER_EVERY: Duration = Duration::from_secs(60);
    const RSS_SLACK_KB: u64 = 200_000;
    const UPLOAD_MBIT: f64 = 3.0;
    const IDLE: Duration = Duration::from_secs(120);
    const KEEPALIVE_EVERY: Duration = Duration::from_secs(25);
    const WG_KEEPALIVE_BYTES: usize = 32;

    let env = step("setup", vpn_env()).await?;
    let upload_end = spawn_tcp_server().await?;
    let mut link = session_link(open(&env, upload_end.target()).await?);
    let call_end = udp_service::spawn(Mode::Sink).await?;
    let bulk_end = udp_service::spawn(Mode::Echo).await?;
    let (mut crx, mut ctx) = open_warm(&env, &call_end).await?;
    let (mut brx, mut btx) = open_warm(&env, &bulk_end).await?;
    let idle_end = udp_service::spawn(Mode::Sink).await?;
    let (mut irx, mut itx) = tokio::io::split(open(&env, idle_end.target()).await?);
    let rss_before = rss_kb()?;
    let upload_bytes = (UPLOAD_MBIT * 1e6 / 8.0 * SOAK.as_secs_f64()) as u64;
    let (call, upload, transfers, resumed) = tokio::join!(
        drive(&mut crx, &mut ctx, SOAK, CALL_BITS, 0, PROBE_DATAGRAM),
        tcp_transfer(
            &mut link,
            true,
            upload_bytes,
            Some(UPLOAD_MBIT),
            "soak upload"
        ),
        async {
            let until = Instant::now() + SOAK;
            let mut n = 0;
            while Instant::now() + TRANSFER_EVERY <= until {
                tokio::time::sleep(TRANSFER_EVERY).await;
                n += 1;
                bulk(
                    &mut brx,
                    &mut btx,
                    BULK_BYTES,
                    WG_DATAGRAM,
                    ARRIVAL_MIN_PCT,
                    &format!("soak transfer {n}"),
                )
                .await?;
            }
            anyhow::Ok(())
        },
        async {
            let until = Instant::now() + IDLE;
            while Instant::now() < until {
                itx.write_all(&[0x55; WG_KEEPALIVE_BYTES]).await?;
                let left = until.saturating_duration_since(Instant::now());
                tokio::time::sleep(KEEPALIVE_EVERY.min(left)).await;
            }
            drive(
                &mut irx,
                &mut itx,
                Duration::from_secs(30),
                CALL_BITS,
                0,
                PROBE_DATAGRAM,
            )
            .await
        }
    );
    let rss_after = rss_kb()?;
    step("transfers", async { transfers }).await?;
    step("upload", async { upload?.check(0.9 * UPLOAD_MBIT) }).await?;
    step("call after idle", async {
        let w = resumed?;
        w.check("call after idle", w.call_loss_pct())
    })
    .await?;
    step("call", async {
        let w = call?;
        anyhow::ensure!(
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM) >= SAMPLE_MIN_PCT,
            "the call sent only {:.0} % of its packets",
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM)
        );
        w.check("soak call", w.call_loss_pct())
    })
    .await?;
    step("memory", async {
        anyhow::ensure!(
            rss_after < 2 * rss_before + RSS_SLACK_KB,
            "RSS grew from {rss_before} kB to {rss_after} kB"
        );
        Ok(())
    })
    .await
}
