//! A local UDP service the Exit forwards a Session to, for traffic shapes that are not symmetric.
//!
//! # Why this exists
//!
//! Every other scenario in this crate targets `SessionTarget::ExitNode(0)`, the Exit's built-in
//! loopback, which echoes each byte back. That makes a Session byte-symmetric by construction —
//! and PIX is paid for the **return** direction only, one share per return packet. So the two
//! shapes that bear hardest on it cannot be expressed against a loopback at all:
//!
//! * a **download** is heavy in the direction PIX bills, so its cycles should complete on
//!   application traffic alone;
//! * an **upload** is heavy in the direction PIX does *not* bill, so almost nothing comes back and
//!   the cycle only completes if the Exit fills it — which is the whole of what fill is for.
//!
//! Pointing the Session at `SessionTarget::UdpStream` instead gives the Exit an ordinary socket to
//! forward to. The cluster permits it: `hoprd-localcluster` generates node configs with
//! `use_target_allow_list: false`, so no allow-list stands in the way. Every node runs on
//! localhost, so the Exit reaches this service at `127.0.0.1`.
//!
//! # Why not the repo's `echo-service/`
//!
//! `echo-service/src/udp-{download,upload}.ts` implements exactly these two behaviours and is
//! already parameterised over the wire. It is also Node, built with `tsc`, and reaching it from a
//! Rust scenario would put npm on the path of a test whose subject is a PIX cycle. The k6 load
//! tests need a deployable container; this needs sixty lines and no toolchain.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// The byte [`Mode::Push`] fills every datagram with.
///
/// Public so a reader can check what it received against it. A constant rather than a sequence, so
/// the check it supports is corruption and not ordering — a reader that wants to prove ordering
/// needs this to become a counter, and `tests/pix_shapes.rs::drain_for` says so where it matters.
pub const PUSH_FILL: u8 = 0xAB;

/// A datagram starting with this is echoed back in every mode, so a latency or call probe can share
/// a Session with a push or a sink.
pub const PROBE_MAGIC: [u8; 4] = *b"HPRB";

/// What the service does with what it receives.
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Discard everything, reply with nothing.
    ///
    /// The upload shape. The Session's return direction then carries only what the *protocol*
    /// generates — acknowledgements, SURB-level keep-alives and Exit fill — which is precisely the
    /// case a PIX cycle cannot complete on without help.
    Sink,
    /// Reply to the first datagram with a sustained push, ignoring anything further.
    ///
    /// The download shape. `datagram` bytes every `1/rate` of a second until `total_bytes` have
    /// gone out, which is how `echo-service`'s `udp-download.ts` is parameterised too — a rate and
    /// a segment size rather than a burst, so the return direction carries a *stream* the Exit's
    /// SURB balancer can pace against rather than one arrival.
    Push {
        datagram: usize,
        rate: u64,
        total_bytes: u64,
    },
    /// Echo every datagram, and answer a [`push_request`] with the push it describes.
    ///
    /// A VPN's far end: one Session carries a call, bulk transfers and downloads, one after another.
    Echo,
}

/// Starts a push of `datagram`-sized [`PUSH_FILL`] datagrams at `rate` per second on an [`Mode::Echo`]
/// service, until `total_bytes` have gone out.
pub fn push_request(datagram: usize, rate: u64, total_bytes: u64) -> Vec<u8> {
    [
        &PUSH_REQUEST_MAGIC[..],
        &(datagram as u64).to_le_bytes(),
        &rate.to_le_bytes(),
        &total_bytes.to_le_bytes(),
    ]
    .concat()
}

const PUSH_REQUEST_MAGIC: [u8; 4] = *b"HPSH";

fn parse_push_request(datagram: &[u8]) -> Option<(usize, u64, u64)> {
    let rest = datagram.strip_prefix(&PUSH_REQUEST_MAGIC)?;
    let field = |i: usize| {
        Some(u64::from_le_bytes(
            rest.get(i * 8..i * 8 + 8)?.try_into().ok()?,
        ))
    };
    Some((field(0)? as usize, field(1)?, field(2)?))
}

/// A running service. Aborted on drop, so a scenario cannot leak one into the next.
pub struct UdpService {
    addr: SocketAddr,
    abort: futures::stream::AbortHandle,
    received: Arc<AtomicU64>,
    sent: Arc<AtomicU64>,
}

impl UdpService {
    /// Where the Exit should be told to forward to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The target spec for [`crate::SessionTarget::UdpStream`].
    ///
    /// `SealedHost::Plain` because the Exit is a cluster node this test also controls; sealing a
    /// target hides it from the *relay*, which is not a property any scenario here measures.
    pub fn target(&self) -> crate::SessionTarget {
        use edgli::hopr_lib::exports::network::types::prelude::{IpOrHost, SealedHost};
        crate::SessionTarget::UdpStream(SealedHost::Plain(IpOrHost::Ip(self.addr)))
    }

    /// Bytes the service has received from the Exit — the Entry → Exit direction.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Bytes the service has pushed towards the Exit — the direction PIX bills.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
}

impl Drop for UdpService {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Bind a service on an ephemeral localhost port and start it.
///
/// The port is ephemeral rather than fixed because two scenarios must be able to run back to back
/// without the second inheriting the first's socket — the cluster's own fixed ports already force
/// scenarios apart, and a fixed port here would add a second reason for a rerun to fail that has
/// nothing to do with PIX.
pub async fn spawn(mode: Mode) -> anyhow::Result<UdpService> {
    let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
    let addr = socket.local_addr()?;
    let received = Arc::new(AtomicU64::new(0));
    let sent = Arc::new(AtomicU64::new(0));

    let (abort, registration) = futures::stream::AbortHandle::new_pair();
    let task = {
        let socket = socket.clone();
        let received = received.clone();
        let sent = sent.clone();
        async move {
            // One MTU of headroom over the largest datagram a Session frame becomes.
            let mut buf = vec![0u8; 4096];
            let mut pushing = false;
            // Owned by this task, so aborting the service drops the set and aborts every push.
            let mut pushes = tokio::task::JoinSet::new();
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                received.fetch_add(len as u64, Ordering::Relaxed);

                let request = &buf[..len];
                match mode {
                    // Echoes stay out of `sent`, which counts the push alone.
                    _ if request.starts_with(&PROBE_MAGIC) => {
                        let _ = socket.send_to(request, peer).await;
                    }
                    Mode::Echo => match parse_push_request(request) {
                        Some((datagram, rate, total_bytes)) => {
                            pushes.spawn(push(
                                socket.clone(),
                                peer,
                                datagram,
                                rate,
                                total_bytes,
                                sent.clone(),
                            ));
                        }
                        None => {
                            let _ = socket.send_to(request, peer).await;
                        }
                    },
                    // Latched: the client's request is one datagram, and a second request must not
                    // start a second push racing the first down the same Session.
                    Mode::Push {
                        datagram,
                        rate,
                        total_bytes,
                    } if !pushing => {
                        pushing = true;
                        pushes.spawn(push(
                            socket.clone(),
                            peer,
                            datagram,
                            rate,
                            total_bytes,
                            sent.clone(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    };

    tokio::spawn(futures::future::Abortable::new(task, registration));
    tracing::info!(%addr, ?mode, "UDP service listening");

    Ok(UdpService {
        addr,
        abort,
        received,
        sent,
    })
}

async fn push(
    socket: Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    datagram: usize,
    rate: u64,
    total_bytes: u64,
    sent: Arc<AtomicU64>,
) {
    tracing::info!(%peer, datagram, rate, total_bytes, "UDP service: starting push");
    let payload = vec![PUSH_FILL; datagram];
    let period = Duration::from_micros(1_000_000 / rate.max(1));
    let mut ticker = tokio::time::interval(period);
    // Hold the long-run average rather than drifting when a send is slow: the
    // rate is what the scenario is pacing its cycle against.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut total = 0u64;
    while total < total_bytes {
        ticker.tick().await;
        match socket.send_to(&payload, peer).await {
            Ok(n) => {
                total += n as u64;
                sent.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(error) => {
                tracing::warn!(%error, "UDP service: push send failed");
                break;
            }
        }
    }
    tracing::info!(total, "UDP service: push finished");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink receives and answers nothing — the property the upload shape rests on.
    #[tokio::test]
    async fn a_sink_absorbs_without_replying() -> anyhow::Result<()> {
        let service = spawn(Mode::Sink).await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        client.send_to(&[1u8; 512], service.addr()).await?;

        let mut buf = [0u8; 64];
        let replied = tokio::time::timeout(Duration::from_millis(300), client.recv(&mut buf)).await;
        assert!(replied.is_err(), "a sink must not reply");

        // The receive is asynchronous, so allow it a moment to be booked.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(512, service.received());
        assert_eq!(0, service.sent());
        Ok(())
    }

    #[tokio::test]
    async fn a_sink_echoes_a_probe_without_counting_it_as_pushed() -> anyhow::Result<()> {
        let service = spawn(Mode::Sink).await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let probe = [&PROBE_MAGIC[..], &[7u8; 60]].concat();
        client.send_to(&probe, service.addr()).await?;

        let mut buf = [0u8; 128];
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await??;
        assert_eq!(probe, buf[..n]);
        assert_eq!(0, service.sent());
        Ok(())
    }

    #[tokio::test]
    async fn an_echo_echoes_and_pushes_on_request() -> anyhow::Result<()> {
        let service = spawn(Mode::Echo).await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let mut buf = [0u8; 256];

        client.send_to(b"ping", service.addr()).await?;
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await??;
        assert_eq!(b"ping", &buf[..n]);

        client
            .send_to(&push_request(100, 200, 300), service.addr())
            .await?;
        let mut got = 0;
        while got < 300 {
            let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await??;
            assert!(buf[..n].iter().all(|b| *b == PUSH_FILL));
            got += n;
        }
        assert_eq!(300, service.sent());
        Ok(())
    }

    #[tokio::test]
    async fn a_probe_does_not_trigger_a_push() -> anyhow::Result<()> {
        let service = spawn(Mode::Push {
            datagram: 100,
            rate: 200,
            total_bytes: 1_000,
        })
        .await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let probe = [&PROBE_MAGIC[..], &[7u8; 60]].concat();
        client.send_to(&probe, service.addr()).await?;

        let mut buf = [0u8; 128];
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await??;
        assert_eq!(probe, buf[..n]);
        assert_eq!(0, service.sent());
        Ok(())
    }

    #[tokio::test]
    async fn dropping_the_service_stops_its_push() -> anyhow::Result<()> {
        let service = spawn(Mode::Echo).await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        client
            .send_to(&push_request(100, 200, 1_000_000), service.addr())
            .await?;
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await??;
        drop(service);

        tokio::time::sleep(Duration::from_millis(100)).await;
        while client.try_recv(&mut buf).is_ok() {}
        let after = tokio::time::timeout(Duration::from_millis(400), client.recv(&mut buf)).await;
        assert!(after.is_err(), "the push must stop with its service");
        Ok(())
    }

    /// A push answers one request with a sustained stream, and stops at the total it was given.
    #[tokio::test]
    async fn a_push_streams_until_its_total_is_spent() -> anyhow::Result<()> {
        let service = spawn(Mode::Push {
            datagram: 100,
            rate: 200,
            total_bytes: 1_000,
        })
        .await?;
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        client.send_to(b"go", service.addr()).await?;

        let mut got = 0u64;
        let mut buf = [0u8; 256];
        while got < 1_000 {
            match tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await {
                Ok(Ok(n)) => got += n as u64,
                _ => break,
            }
        }
        assert_eq!(1_000, got, "the push must deliver exactly its total");

        // And stop there rather than streaming forever.
        let mut extra = [0u8; 256];
        let after = tokio::time::timeout(Duration::from_millis(400), client.recv(&mut extra)).await;
        assert!(after.is_err(), "the push must stop at its total");
        Ok(())
    }
}
