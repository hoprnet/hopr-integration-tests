//! In-process userspace UDP shaper and Blokli HTTP reverse proxy.
//!
//! Enables controlled network impairment (latency, blackhole, packet loss)
//! on the QUIC transport connection between Edgli and cluster nodes without
//! requiring root privileges (`tc netem` / `iptables`).

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use tokio::{net::UdpSocket, sync::mpsc, time::Instant};

/// Configuration and runtime controls for a UDP shaper instance.
pub struct ShaperControl {
    delay_micros: AtomicU64,
    blackhole: AtomicBool,
    drop_every: AtomicU32,
    drop_counter: AtomicU64,
    loss_rate_ppm: AtomicU32, // parts per million (0..1_000_000)

    forward_received: AtomicU64,
    forward_sent: AtomicU64,
    forward_dropped: AtomicU64,

    return_received: AtomicU64,
    return_sent: AtomicU64,
    return_dropped: AtomicU64,
}

impl Default for ShaperControl {
    fn default() -> Self {
        Self {
            delay_micros: AtomicU64::new(0),
            blackhole: AtomicBool::new(false),
            drop_every: AtomicU32::new(0), // 0 means disabled
            drop_counter: AtomicU64::new(0),
            loss_rate_ppm: AtomicU32::new(0),

            forward_received: AtomicU64::new(0),
            forward_sent: AtomicU64::new(0),
            forward_dropped: AtomicU64::new(0),

            return_received: AtomicU64::new(0),
            return_sent: AtomicU64::new(0),
            return_dropped: AtomicU64::new(0),
        }
    }
}

/// Snapshot of packet statistics for a shaper instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaperStats {
    pub forward_received: u64,
    pub forward_sent: u64,
    pub forward_dropped: u64,
    pub return_received: u64,
    pub return_sent: u64,
    pub return_dropped: u64,
}

impl ShaperControl {
    pub fn new(delay: Duration) -> Self {
        let control = Self::default();
        control.set_delay(delay);
        control
    }

    pub fn set_delay(&self, delay: Duration) {
        self.delay_micros
            .store(delay.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn delay(&self) -> Duration {
        Duration::from_micros(self.delay_micros.load(Ordering::Relaxed))
    }

    pub fn set_blackhole(&self, enabled: bool) {
        self.blackhole.store(enabled, Ordering::SeqCst);
    }

    pub fn is_blackhole(&self) -> bool {
        self.blackhole.load(Ordering::SeqCst)
    }

    pub fn set_drop_every(&self, interval: Option<u32>) {
        self.drop_every
            .store(interval.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn set_loss_rate(&self, rate: f64) {
        let ppm = (rate.clamp(0.0, 1.0) * 1_000_000.0) as u32;
        self.loss_rate_ppm.store(ppm, Ordering::Relaxed);
    }

    pub fn stats(&self) -> ShaperStats {
        ShaperStats {
            forward_received: self.forward_received.load(Ordering::Relaxed),
            forward_sent: self.forward_sent.load(Ordering::Relaxed),
            forward_dropped: self.forward_dropped.load(Ordering::Relaxed),
            return_received: self.return_received.load(Ordering::Relaxed),
            return_sent: self.return_sent.load(Ordering::Relaxed),
            return_dropped: self.return_dropped.load(Ordering::Relaxed),
        }
    }

    fn should_drop(&self) -> bool {
        if self.blackhole.load(Ordering::SeqCst) {
            return true;
        }

        let interval = self.drop_every.load(Ordering::Relaxed);
        if interval > 0 {
            let count = self.drop_counter.fetch_add(1, Ordering::Relaxed);
            if count.is_multiple_of(interval as u64) {
                return true;
            }
        }

        let ppm = self.loss_rate_ppm.load(Ordering::Relaxed);
        if ppm > 0 {
            let rnd = rand::random::<u32>() % 1_000_000;
            if rnd < ppm {
                return true;
            }
        }

        false
    }
}

/// A UDP shaper relaying packets between a client and a target with artificial delay and loss.
pub struct QuicShaper {
    local_addr: SocketAddr,
    target_addr: SocketAddr,
    control: Arc<ShaperControl>,
    _abort_handles: Vec<futures::future::AbortHandle>,
}

impl QuicShaper {
    pub async fn bind(target_addr: SocketAddr, delay: Duration) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("binding UDP shaper socket")?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);

        let control = Arc::new(ShaperControl::new(delay));
        let client_addr = Arc::new(RwLock::new(None::<SocketAddr>));

        let (tx, mut rx) = mpsc::channel::<(Instant, Vec<u8>, SocketAddr)>(4096);

        // Receiver task
        let recv_socket = socket.clone();
        let recv_control = control.clone();
        let recv_client = client_addr.clone();
        let (recv_future, recv_handle) = futures::future::abortable(async move {
            let mut buf = [0u8; 65536];
            loop {
                let (len, src) = match recv_socket.recv_from(&mut buf).await {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::debug!(%err, "shaper socket recv error");
                        break;
                    }
                };

                let data = buf[..len].to_vec();
                let now = Instant::now();
                let delay = recv_control.delay();
                let send_time = now + delay;

                if src == target_addr {
                    // Return packet (target -> client)
                    recv_control.return_received.fetch_add(1, Ordering::Relaxed);
                    if recv_control.should_drop() {
                        recv_control.return_dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let client = *recv_client.read().unwrap();
                    if let Some(client) = client
                        && tx.send((send_time, data, client)).await.is_err()
                    {
                        break;
                    }
                } else {
                    // Forward packet (client -> target)
                    recv_control
                        .forward_received
                        .fetch_add(1, Ordering::Relaxed);
                    *recv_client.write().unwrap() = Some(src);

                    if recv_control.should_drop() {
                        recv_control.forward_dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    if tx.send((send_time, data, target_addr)).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Sender task
        let send_socket = socket.clone();
        let send_control = control.clone();
        let (send_future, send_handle) = futures::future::abortable(async move {
            let mut queue: VecDeque<(Instant, Vec<u8>, SocketAddr)> = VecDeque::new();
            loop {
                // Drain any items whose delivery deadline has passed
                let now = Instant::now();
                while let Some(front) = queue.front() {
                    if front.0 <= now {
                        let (_, data, dest) = queue.pop_front().unwrap();
                        let is_return = dest != target_addr;
                        if send_socket.send_to(&data, dest).await.is_ok() {
                            if is_return {
                                send_control.return_sent.fetch_add(1, Ordering::Relaxed);
                            } else {
                                send_control.forward_sent.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        break;
                    }
                }

                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(item) => queue.push_back(item),
                            None => break,
                        }
                    }
                    _ = async {
                        if let Some(front) = queue.front() {
                            tokio::time::sleep_until(front.0).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    }, if !queue.is_empty() => {}
                }
            }
        });

        tokio::spawn(recv_future);
        tokio::spawn(send_future);

        Ok(Self {
            local_addr,
            target_addr,
            control,
            _abort_handles: vec![recv_handle, send_handle],
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn local_port(&self) -> u16 {
        self.local_addr.port()
    }

    pub fn target_addr(&self) -> SocketAddr {
        self.target_addr
    }

    pub fn control(&self) -> &Arc<ShaperControl> {
        &self.control
    }
}

impl Drop for QuicShaper {
    fn drop(&mut self) {
        for handle in &self._abort_handles {
            handle.abort();
        }
    }
}

/// A pool of UDP shapers covering all cluster nodes in a full mesh.
pub struct QuicShaperPool {
    shapers: Vec<QuicShaper>,
    port_mappings: Vec<(u16, u16)>,
}

impl QuicShaperPool {
    pub async fn bind(cluster_size: usize, p2p_port_base: u16) -> anyhow::Result<Self> {
        let mut shapers = Vec::with_capacity(cluster_size);
        let mut port_mappings = Vec::with_capacity(cluster_size);

        for i in 0..cluster_size {
            let target_port = p2p_port_base + i as u16;
            let target_addr: SocketAddr = format!("127.0.0.1:{target_port}").parse()?;
            let shaper = QuicShaper::bind(target_addr, Duration::ZERO).await?;
            port_mappings.push((target_port, shaper.local_port()));
            shapers.push(shaper);
        }

        tracing::info!(?port_mappings, "QUIC shaper pool bound");
        Ok(Self {
            shapers,
            port_mappings,
        })
    }

    pub fn port_mappings(&self) -> &[(u16, u16)] {
        &self.port_mappings
    }

    pub fn set_delay(&self, delay: Duration) {
        for shaper in &self.shapers {
            shaper.control().set_delay(delay);
        }
    }

    pub fn set_blackhole(&self, enabled: bool) {
        for shaper in &self.shapers {
            shaper.control().set_blackhole(enabled);
        }
    }

    pub fn set_drop_every(&self, interval: Option<u32>) {
        for shaper in &self.shapers {
            shaper.control().set_drop_every(interval);
        }
    }

    pub fn set_loss_rate(&self, rate: f64) {
        for shaper in &self.shapers {
            shaper.control().set_loss_rate(rate);
        }
    }

    pub fn stats(&self) -> Vec<ShaperStats> {
        self.shapers.iter().map(|s| s.control().stats()).collect()
    }
}

/// Rewrites UDP multiaddresses in strings (e.g. `/udp/19000/` -> `/udp/54321/`).
pub fn rewrite_ports(input: &str, mappings: &[(u16, u16)]) -> String {
    let mut result = input.to_string();
    for &(from_port, to_port) in mappings {
        let from_slash = format!("/udp/{from_port}/");
        let to_slash = format!("/udp/{to_port}/");
        result = result.replace(&from_slash, &to_slash);

        let from_bare = format!("/udp/{from_port}");
        let to_bare = format!("/udp/{to_port}");
        result = result.replace(&from_bare, &to_bare);
    }
    result
}

/// A lightweight HTTP reverse proxy that intercepts Blokli responses and rewrites node multiaddresses.
pub struct BlokliProxy {
    local_port: u16,
    _abort_handle: futures::future::AbortHandle,
}

impl BlokliProxy {
    pub async fn start(upstream_url: &str, mappings: Vec<(u16, u16)>) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding BlokliProxy listener")?;
        let local_port = listener.local_addr()?.port();
        let upstream = upstream_url.trim_end_matches('/').to_string();
        let mappings = Arc::new(mappings);
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        let (server_future, abort_handle) = futures::future::abortable(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::debug!(%err, "proxy accept error");
                        break;
                    }
                };

                let upstream = upstream.clone();
                let mappings = mappings.clone();
                let http_client = http_client.clone();

                tokio::spawn(async move {
                    if let Err(err) =
                        handle_proxy_conn(stream, &upstream, &mappings, &http_client).await
                    {
                        tracing::debug!(%err, "proxy connection handler error");
                    }
                });
            }
        });

        tokio::spawn(server_future);
        tracing::info!(local_port, upstream = %upstream_url, "Blokli reverse proxy listening");

        Ok(Self {
            local_port,
            _abort_handle: abort_handle,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for BlokliProxy {
    fn drop(&mut self) {
        self._abort_handle.abort();
    }
}

async fn handle_proxy_conn(
    stream: tokio::net::TcpStream,
    upstream: &str,
    mappings: &[(u16, u16)],
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        anyhow::bail!("malformed HTTP request line: {request_line}");
    }
    let method = parts[0];
    let path = parts[1];

    let mut headers = Vec::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim();
            if key == "content-length" {
                content_length = val.parse::<usize>().unwrap_or(0);
            }
            headers.push((key, val.to_string()));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let target_url = format!("{upstream}{path}");
    let mut req = match method {
        "POST" => client.post(&target_url).body(body),
        "GET" => client.get(&target_url),
        other => client.request(other.parse()?, &target_url),
    };

    for (k, v) in headers {
        if k != "host" && k != "content-length" {
            req = req.header(k, v);
        }
    }

    let resp = req.send().await?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();

    let is_sse = content_type.contains("text/event-stream");
    if is_sse {
        use futures::StreamExt as _;

        const HEADER: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        writer.write_all(HEADER.as_bytes()).await?;
        writer.flush().await?;

        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let text = String::from_utf8_lossy(&chunk);
            let rewritten = rewrite_ports(&text, mappings);
            writer.write_all(rewritten.as_bytes()).await?;
            writer.flush().await?;
        }
    } else {
        let resp_body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&resp_body);
        let rewritten = rewrite_ports(&text, mappings);
        let rewritten_bytes = rewritten.as_bytes();

        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("OK"),
            content_type,
            rewritten_bytes.len()
        );
        writer.write_all(header.as_bytes()).await?;
        writer.write_all(rewritten_bytes).await?;
        writer.flush().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_ports_replaces_multiaddr_patterns() {
        let input = r#"{"data":{"peer":"/ip4/127.0.0.1/udp/19000/quic-v1/p2p/12D3","other":"/ip4/127.0.0.1/udp/19001"}}"#;
        let mappings = vec![(19000, 54000), (19001, 54001)];
        let rewritten = rewrite_ports(input, &mappings);
        assert_eq!(
            rewritten,
            r#"{"data":{"peer":"/ip4/127.0.0.1/udp/54000/quic-v1/p2p/12D3","other":"/ip4/127.0.0.1/udp/54001"}}"#
        );
    }

    #[tokio::test]
    async fn udp_shaper_relays_packets_both_ways() -> anyhow::Result<()> {
        let target_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let target_addr = target_sock.local_addr()?;

        let shaper = QuicShaper::bind(target_addr, Duration::ZERO).await?;
        let client_sock = UdpSocket::bind("127.0.0.1:0").await?;

        // Client -> Shaper -> Target
        client_sock
            .send_to(b"hello target", shaper.local_addr())
            .await?;
        let mut buf = [0u8; 128];
        let (n, src) = target_sock.recv_from(&mut buf).await?;
        assert_eq!(&buf[..n], b"hello target");
        assert_eq!(src, shaper.local_addr());

        // Target -> Shaper -> Client
        target_sock.send_to(b"hello client", src).await?;
        let (n, src) = client_sock.recv_from(&mut buf).await?;
        assert_eq!(&buf[..n], b"hello client");
        assert_eq!(src, shaper.local_addr());

        let stats = shaper.control().stats();
        assert_eq!(stats.forward_sent, 1);
        assert_eq!(stats.return_sent, 1);
        assert_eq!(stats.forward_dropped, 0);

        Ok(())
    }

    #[tokio::test]
    async fn udp_shaper_delays_packets() -> anyhow::Result<()> {
        let target_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let target_addr = target_sock.local_addr()?;

        let delay = Duration::from_millis(30);
        let shaper = QuicShaper::bind(target_addr, delay).await?;
        let client_sock = UdpSocket::bind("127.0.0.1:0").await?;

        let start = Instant::now();
        client_sock.send_to(b"ping", shaper.local_addr()).await?;
        let mut buf = [0u8; 128];
        let (n, src) = target_sock.recv_from(&mut buf).await?;
        let elapsed_fwd = start.elapsed();
        assert_eq!(&buf[..n], b"ping");
        assert!(
            elapsed_fwd >= Duration::from_millis(25),
            "forward delay too short: {elapsed_fwd:?}"
        );

        target_sock.send_to(b"pong", src).await?;
        let (n, _) = client_sock.recv_from(&mut buf).await?;
        let total_elapsed = start.elapsed();
        assert_eq!(&buf[..n], b"pong");
        assert!(
            total_elapsed >= Duration::from_millis(50),
            "round trip delay too short: {total_elapsed:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn udp_shaper_drops_packets_when_blackhole_active() -> anyhow::Result<()> {
        let target_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let target_addr = target_sock.local_addr()?;

        let shaper = QuicShaper::bind(target_addr, Duration::ZERO).await?;
        let client_sock = UdpSocket::bind("127.0.0.1:0").await?;

        shaper.control().set_blackhole(true);
        client_sock
            .send_to(b"into the void", shaper.local_addr())
            .await?;

        let mut buf = [0u8; 128];
        let res =
            tokio::time::timeout(Duration::from_millis(50), target_sock.recv_from(&mut buf)).await;
        assert!(res.is_err(), "target should not receive blackholed packet");

        let stats = shaper.control().stats();
        assert_eq!(stats.forward_dropped, 1);
        assert_eq!(stats.forward_sent, 0);

        Ok(())
    }

    #[tokio::test]
    async fn udp_shaper_deterministic_drop_pattern() -> anyhow::Result<()> {
        let target_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let target_addr = target_sock.local_addr()?;

        let shaper = QuicShaper::bind(target_addr, Duration::ZERO).await?;
        let client_sock = UdpSocket::bind("127.0.0.1:0").await?;

        // Drop every 2nd packet (0, 2, 4...)
        shaper.control().set_drop_every(Some(2));

        for i in 0..6 {
            client_sock.send_to(&[i as u8], shaper.local_addr()).await?;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = shaper.control().stats();
        assert_eq!(stats.forward_received, 6);
        assert_eq!(stats.forward_dropped, 3);
        assert_eq!(stats.forward_sent, 3);

        Ok(())
    }

    #[tokio::test]
    async fn blokli_proxy_rewrites_http_response() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Mock upstream Blokli server
        let mock_server = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream_url = format!("http://{}", mock_server.local_addr()?);

        tokio::spawn(async move {
            let (mut socket, _) = mock_server.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"nodes":[{"p2p":"/ip4/127.0.0.1/udp/19000/quic-v1"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(resp.as_bytes()).await.unwrap();
        });

        let proxy = BlokliProxy::start(&upstream_url, vec![(19000, 39999)]).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/graphql", proxy.url()))
            .send()
            .await?;
        let text = resp.text().await?;

        assert!(
            text.contains("/udp/39999/quic-v1"),
            "expected rewritten port 39999, got: {text}"
        );
        assert!(
            !text.contains("/udp/19000/"),
            "original port 19000 should be rewritten"
        );

        Ok(())
    }
}
