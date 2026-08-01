use ipnet::IpNet;
use std::net::IpAddr;

pub(crate) fn parse_client_ip(s: &str) -> std::result::Result<IpNet, String> {
    if !s.contains('/') {
        let ip: IpAddr = s
            .parse()
            .map_err(|e: std::net::AddrParseError| e.to_string())?;
        Ok(IpNet::from(ip))
    } else {
        s.parse().map_err(|e: ipnet::AddrParseError| e.to_string())
    }
}

/// Resolve the effective client IP for blacklist / `client_ip` routing:
/// the direct peer unless it is a trusted proxy, in which case walk
/// X-Forwarded-For right-to-left and return the first hop not covered by a
/// trusted CIDR. Malformed hops are skipped; if every hop is trusted or
/// unparsable, the direct peer is returned as a fallback.
pub(crate) fn effective_client_ip(
    peer_ip: Option<IpAddr>,
    xff: Option<&str>,
    trusted: &[IpNet],
) -> Option<IpAddr> {
    let peer = peer_ip?;
    if trusted.iter().any(|n| n.contains(&peer)) {
        if let Some(header) = xff {
            for hop in header.split(',').rev() {
                let hop = hop.trim();
                if let Ok(ip) = hop.parse::<IpAddr>() {
                    if !trusted.iter().any(|n| n.contains(&ip)) {
                        return Some(ip);
                    }
                }
            }
        }
        // All hops trusted (or unusable): fall back to the peer itself.
        Some(peer)
    } else {
        Some(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_client_ip_accepts_single_ip_and_cidr() {
        let single = parse_client_ip("10.0.0.1").unwrap();
        assert_eq!(single.prefix_len(), 32, "bare IP becomes a /32");
        let single_ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(single.contains(&single_ip));

        let cidr = parse_client_ip("10.0.0.0/8").unwrap();
        let inside: IpAddr = "10.9.9.9".parse().unwrap();
        let outside: IpAddr = "11.0.0.1".parse().unwrap();
        assert!(cidr.contains(&inside));
        assert!(!cidr.contains(&outside));

        assert!(parse_client_ip("not-an-ip").is_err());
        assert!(parse_client_ip("10.0.0.0/99").is_err());
    }

    #[test]
    fn effective_client_ip_trusts_xff_only_behind_trusted_proxy() {
        let trusted: Vec<IpNet> = ["10.0.0.0/8"]
            .iter()
            .map(|s| parse_client_ip(s).unwrap())
            .collect();
        let proxy: IpAddr = "10.0.0.5".parse().unwrap();
        let client: IpAddr = "203.0.113.9".parse().unwrap();
        let spoof: IpAddr = "198.51.100.7".parse().unwrap();

        // Direct client (not a trusted proxy): XFF ignored.
        assert_eq!(
            effective_client_ip(Some(client), Some("1.2.3.4"), &trusted),
            Some(client)
        );
        // Behind the trusted proxy: rightmost untrusted XFF hop wins.
        assert_eq!(
            effective_client_ip(
                Some(proxy),
                Some("1.2.3.4, 10.0.0.9, 203.0.113.9"),
                &trusted
            ),
            Some(client)
        );
        // Untrusted hop present but a spoof attempt comes last after the proxy.
        assert_eq!(
            effective_client_ip(Some(proxy), Some("198.51.100.7, 10.0.0.9"), &trusted),
            Some(spoof),
            "untrusted hops are honored even when the proxy appended itself"
        );
        // All hops trusted / unparsable: fall back to the peer.
        assert_eq!(
            effective_client_ip(Some(proxy), Some("10.0.0.8, 10.0.0.9"), &trusted),
            Some(proxy)
        );
        assert_eq!(
            effective_client_ip(Some(proxy), Some("garbage"), &trusted),
            Some(proxy)
        );
        // No peer: no client.
        assert_eq!(effective_client_ip(None, Some("1.2.3.4"), &trusted), None);
    }
}
