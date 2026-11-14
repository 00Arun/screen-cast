//! Filtering proxy for the sandboxed browser.
//!
//! All website traffic (pages, iframes, scripts, XHR/fetch, WebSockets, media) is sent through
//! this proxy on 127.0.0.1. It resolves DNS itself and refuses any destination whose *resolved*
//! address is not on the public internet (loopback, private LAN, link-local, CGNAT, multicast,
//! ...), then connects to exactly the address it checked. This stops websites from reaching
//! your router, NAS, local dev servers or this app's own cast server, including tricks like
//! `127.0.0.1.nip.io`, `localhost.`, attacker DNS pointing at 192.168.x.x, or DNS rebinding.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    time::{timeout, Duration},
};

const MAX_HEADER: usize = 64 * 1024;

/// `true` only for addresses on the public internet.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_public_v4(v),
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v.segments();
            !(v.is_unspecified()
                || v.is_loopback()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x64 && s[1] == 0xff9b) // NAT64 (could embed private v4)
                || (s[0] == 0x2002)) // 6to4 (embeds v4)
        }
    }
}

fn is_public_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    !(v.is_unspecified()
        || v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        || v.is_multicast()
        || v.is_broadcast()
        || o[0] == 0 // "this network"
        || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT / Tailscale
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (18..=19).contains(&o[1])) // benchmarking
        || o[0] >= 240) // reserved
}

/// Resolves `host:port` and returns the first public address, or an error message.
pub async fn resolve_public(host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port)).await.map_err(|e| e.to_string())?.collect(),
    };
    if addrs.is_empty() {
        return Err("no address".into());
    }
    // If ANY answer is non-public, refuse: mixed answers are a rebinding trick.
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(format!("{host} resolves to a local address"));
    }
    Ok(addrs[0])
}

/// Starts the proxy on a random loopback port and returns that port.
pub async fn start() -> std::io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((client, peer)) = listener.accept().await else { continue };
            if !peer.ip().is_loopback() {
                continue;
            }
            tokio::spawn(async move {
                let _ = handle(client).await;
            });
        }
    });
    Ok(port)
}

fn dbg(msg: &str) {
    if std::env::var_os("SC_DEBUG").is_some() {
        eprintln!("[netguard] {msg}");
    }
}

async fn deny(client: &mut TcpStream, why: &str) -> std::io::Result<()> {
    dbg(&format!("BLOCK {why}"));
    let body = format!("Blocked by Screen Cast: {why}\n");
    let resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(resp.as_bytes()).await
}

/// Splits "host:port" (IPv6 in brackets allowed) with a default port.
fn split_host_port(s: &str, default: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = rest[end + 1..].strip_prefix(':').map_or(Some(default), |p| p.parse().ok())?;
        return Some((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default)),
    }
}

async fn handle(mut client: TcpStream) -> std::io::Result<()> {
    // Read the request head (request line + headers).
    let mut reader = BufReader::new(&mut client);
    let mut head = Vec::new();
    loop {
        let n = timeout(Duration::from_secs(15), reader.read_until(b'\n', &mut head)).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "header timeout")
        })??;
        if n == 0 || head.len() > MAX_HEADER {
            return Ok(());
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    // Bytes the client already sent after the head (e.g. a request body) stay in the buffer.
    let buffered = reader.buffer().to_vec();
    drop(reader);

    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, target, version) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or("HTTP/1.1"),
    );

    if method.eq_ignore_ascii_case("CONNECT") {
        // HTTPS / WSS tunnel.
        let Some((host, port)) = split_host_port(target, 443) else { return deny(&mut client, "bad target").await };
        let addr = match resolve_public(&host, port).await {
            Ok(a) => a,
            Err(e) => return deny(&mut client, &e).await,
        };
        let Ok(Ok(mut upstream)) = timeout(Duration::from_secs(15), TcpStream::connect(addr)).await else {
            return client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n").await;
        };
        dbg(&format!("allow CONNECT {host}:{port}"));
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        if !buffered.is_empty() {
            upstream.write_all(&buffered).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    // Plain HTTP (absolute-form request target), including ws:// upgrades.
    let Ok(url) = url::Url::parse(target) else { return deny(&mut client, "bad request").await };
    if url.scheme() != "http" {
        return deny(&mut client, "unsupported scheme").await;
    }
    let Some(host) = url.host_str() else { return deny(&mut client, "no host").await };
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = match resolve_public(host, port).await {
        Ok(a) => a,
        Err(e) => return deny(&mut client, &e).await,
    };
    let Ok(Ok(mut upstream)) = timeout(Duration::from_secs(15), TcpStream::connect(addr)).await else {
        return client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n").await;
    };
    // Forward in origin-form; one request per connection so a reused browser connection can
    // never be steered to a host we did not check.
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    let upgrade = text.to_ascii_lowercase().contains("\r\nupgrade:");
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let name = line.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        if name == "proxy-connection" || name == "proxy-authorization" || (!upgrade && name == "connection") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !upgrade {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    upstream.write_all(out.as_bytes()).await?;
    if !buffered.is_empty() {
        upstream.write_all(&buffered).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    let _ = client.read(&mut [0u8; 0]).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "172.20.0.1", "169.254.1.1", "100.64.0.1",
                   "100.127.255.254", "0.0.0.0", "224.0.0.1", "255.255.255.255", "198.18.0.1", "::1",
                   "fe80::1", "fd00::1", "::ffff:192.168.1.1", "::"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "100.128.0.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn splits_targets() {
        assert_eq!(split_host_port("example.com:443", 80), Some(("example.com".into(), 443)));
        assert_eq!(split_host_port("[::1]:8080", 80), Some(("::1".into(), 8080)));
        assert_eq!(split_host_port("example.com", 80), Some(("example.com".into(), 80)));
    }

    async fn ask(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 256];
        let n = timeout(Duration::from_secs(20), s.read(&mut buf)).await.unwrap().unwrap();
        String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn proxy_blocks_local_destinations() {
        let port = start().await.unwrap();
        // Direct loopback / LAN targets.
        assert!(ask(port, "CONNECT 127.0.0.1:8080 HTTP/1.1\r\nHost: x\r\n\r\n").await.contains("403"));
        assert!(ask(port, "GET http://192.168.1.1/ HTTP/1.1\r\nHost: 192.168.1.1\r\n\r\n").await.contains("403"));
        assert!(ask(port, "GET http://localhost.:8080/ HTTP/1.1\r\nHost: localhost.\r\n\r\n").await.contains("403"));
        // Public names that resolve to private addresses (DNS tricks).
        if std::env::var_os("SC_NET_TESTS").is_some() {
            assert!(ask(port, "CONNECT 127.0.0.1.nip.io:443 HTTP/1.1\r\nHost: x\r\n\r\n").await.contains("403"));
            assert!(ask(port, "GET http://192.168.1.1.nip.io/ HTTP/1.1\r\nHost: 192.168.1.1.nip.io\r\n\r\n").await.contains("403"));
            // A normal public site still works.
            assert!(ask(port, "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n").await.contains("200"));
            let r = ask(port, "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
            assert!(r.starts_with("HTTP/1.1 200") || r.starts_with("HTTP/1.1 30"), "{r}");
        }
    }
}
