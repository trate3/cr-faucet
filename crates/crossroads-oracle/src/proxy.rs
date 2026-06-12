//! PROXY-protocol (v1) front for per-circuit rate limiting over Tor.
//!
//! With `HiddenServiceExportCircuitID haproxy` in the onion service's torrc, Tor
//! prepends a PROXY-protocol v1 line to each connection whose SOURCE address is a
//! synthetic, unique-per-circuit value. We parse that and surface it as the
//! connection's address, so the handler can rate-limit per circuit (not per IP —
//! there are no real IPs over Tor). A hammerer on one circuit only throttles
//! themselves; buying more circuits costs the onion PoW each time.
//!
//! Direct (non-Tor) callers send no PROXY header — we detect that by peeking and
//! fall back to their real peer address, leaving the stream untouched for HTTP.
//!
//! NOTE: Tor's haproxy export uses PROXY protocol **v1** (human-readable). If a
//! future Tor switches to v2 (binary), revisit `read_source`.

use std::io;
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// If the stream begins with a PROXY v1 header, consume it and return the encoded
/// source address (the per-circuit key). Otherwise leave the stream untouched and
/// return `None`.
pub async fn read_source(stream: &mut TcpStream) -> io::Result<Option<SocketAddr>> {
    // Peek (non-consuming): only a real PROXY header starts with "PROXY ".
    let mut head = [0u8; 6];
    let n = stream.peek(&mut head).await?;
    if n < 6 || &head[..6] != b"PROXY " {
        return Ok(None); // direct client → leave bytes for HTTP
    }
    // Confirmed: consume exactly the header line (CRLF-terminated, ≤ 107 bytes).
    let mut line = Vec::with_capacity(108);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") || line.len() > 107 {
            break;
        }
    }
    Ok(parse_v1_source(&line))
}

/// Parse `PROXY TCP4|TCP6 <src> <dst> <sport> <dport>\r\n` → source SocketAddr.
/// `UNKNOWN` or anything malformed → None.
fn parse_v1_source(line: &[u8]) -> Option<SocketAddr> {
    let s = std::str::from_utf8(line).ok()?.trim_end();
    let mut f = s.split(' ');
    if f.next()? != "PROXY" {
        return None;
    }
    let proto = f.next()?;
    if proto != "TCP4" && proto != "TCP6" {
        return None; // UNKNOWN
    }
    let src_ip = f.next()?;
    let _dst_ip = f.next()?;
    let src_port = f.next()?;
    // IPv6 SocketAddr strings need brackets: [addr]:port.
    let addr = if proto == "TCP6" {
        format!("[{src_ip}]:{src_port}")
    } else {
        format!("{src_ip}:{src_port}")
    };
    addr.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tcp6_circuit_source() {
        // Tor encodes the circuit id into the source address; we just key on it.
        let line = b"PROXY TCP6 2001:db8::1 ::1 43210 80\r\n";
        let src = parse_v1_source(line).unwrap();
        assert_eq!(src.port(), 43210);
        assert!(src.is_ipv6());
    }

    #[test]
    fn parses_tcp4_source() {
        let line = b"PROXY TCP4 127.128.0.5 127.0.0.1 12345 80\r\n";
        let src = parse_v1_source(line).unwrap();
        assert_eq!(src.to_string(), "127.128.0.5:12345");
    }

    #[test]
    fn unknown_and_garbage_are_none() {
        assert!(parse_v1_source(b"PROXY UNKNOWN\r\n").is_none());
        assert!(parse_v1_source(b"GET / HTTP/1.1\r\n").is_none());
    }
}
