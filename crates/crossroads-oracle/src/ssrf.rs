//! SSRF guard for source-chain RPC URLs.
//!
//! We sign reports for ANY permissionlessly-deployed oracle that references our
//! registry, and read each oracle's `sourceRpcUrls` from chain. A malicious oracle
//! could list URLs pointing at internal / cloud-metadata addresses to make the TEE
//! fetch from them. So before using any source RPC, we require its scheme to be
//! http(s) and EVERY resolved IP to be globally routable.

use anyhow::{bail, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Reject a URL unless it is http(s) and resolves only to global IPs.
pub async fn assert_public_url(raw: &str) -> Result<()> {
    let u = url::Url::parse(raw).map_err(|e| anyhow::anyhow!("bad url {raw}: {e}"))?;
    match u.scheme() {
        "http" | "https" => {}
        s => bail!("disallowed scheme {s} in {raw}"),
    }
    let host = u.host_str().ok_or_else(|| anyhow::anyhow!("no host in {raw}"))?;
    // A bare IP literal in the URL still gets checked (lookup_host parses it).
    let port = u.port_or_known_default().unwrap_or(443);
    let mut resolved = false;
    for addr in tokio::net::lookup_host((host, port)).await? {
        resolved = true;
        if !is_global(addr.ip()) {
            bail!("non-global IP {} for host {host}", addr.ip());
        }
    }
    if !resolved {
        bail!("host {host} did not resolve");
    }
    Ok(())
}

fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local() // 169.254/16 (incl. cloud metadata 169.254.169.254)
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        || o[0] == 0 // 0.0.0.0/8
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
        || o[0] >= 240) // reserved 240/4
}

fn is_global_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    // Map IPv4-mapped/compatible back to v4 for the same checks.
    if let Some(v4) = v6.to_ipv4() {
        return is_global_v4(v4);
    }
    !(v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80) // link-local fe80::/10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_internal_and_metadata() {
        assert!(!is_global("127.0.0.1".parse().unwrap()));
        assert!(!is_global("10.0.0.5".parse().unwrap()));
        assert!(!is_global("192.168.1.1".parse().unwrap()));
        assert!(!is_global("169.254.169.254".parse().unwrap())); // cloud metadata
        assert!(!is_global("100.64.0.1".parse().unwrap())); // CGNAT
        assert!(!is_global("::1".parse().unwrap()));
        assert!(!is_global("fd00::1".parse().unwrap())); // ULA
        // Public IPs pass.
        assert!(is_global("1.1.1.1".parse().unwrap()));
        assert!(is_global("8.8.8.8".parse().unwrap()));
        assert!(is_global("2606:4700:4700::1111".parse().unwrap()));
    }
}
