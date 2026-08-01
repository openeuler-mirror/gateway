use ipnet::IpNet;
use std::collections::HashSet;
use std::fs;
use std::net::IpAddr;

use crate::client::parse_client_ip;

#[derive(Debug, Clone)]
pub(crate) enum BlacklistEntry {
    Net(IpNet),
    Range { start: IpAddr, end: IpAddr },
}

/// Lookup-optimized blacklist: exact IPs live in an O(1) `HashSet`; CIDR nets
/// and inclusive ranges stay in small Vecs. Parsing happens only on load /
/// reload, so per-request checks are cheap even for very large files.
#[derive(Debug, Clone, Default)]
pub(crate) struct Blacklist {
    exact: HashSet<IpAddr>,
    nets: Vec<IpNet>,
    ranges: Vec<(IpAddr, IpAddr)>,
}

impl Blacklist {
    pub(crate) fn from_entries(entries: Vec<BlacklistEntry>) -> Self {
        let mut bl = Blacklist::default();
        for e in entries {
            match e {
                BlacklistEntry::Net(net) => {
                    // A /32 or /128 net is a single address: hash it for O(1).
                    if net.prefix_len() == net.max_prefix_len() {
                        bl.exact.insert(net.addr());
                    } else {
                        bl.nets.push(net);
                    }
                }
                BlacklistEntry::Range { start, end } => bl.ranges.push((start, end)),
            }
        }
        bl
    }

    /// True if `ip` is covered by any blacklist entry. A range of one address
    /// family never matches a query of the other (IpAddr ordering puts all
    /// IPv4 before all IPv6, so the comparison naturally yields false).
    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        self.exact.contains(&ip)
            || self.nets.iter().any(|n| n.contains(&ip))
            || self.ranges.iter().any(|(s, e)| ip >= *s && ip <= *e)
    }

    /// Total number of entries (exact + nets + ranges).
    pub(crate) fn len(&self) -> usize {
        self.exact.len() + self.nets.len() + self.ranges.len()
    }
}

/// Parse a single range `start-end` (already split) into a `BlacklistEntry`.
/// Both ends must be the same family and `start <= end`.
fn parse_range(start: &str, end: &str) -> std::result::Result<BlacklistEntry, String> {
    let s: IpAddr = start
        .parse()
        .map_err(|e: std::net::AddrParseError| format!("range start '{start}': {e}"))?;
    let e: IpAddr = end
        .parse()
        .map_err(|e: std::net::AddrParseError| format!("range end '{end}': {e}"))?;
    let same_family = s.is_ipv4() == e.is_ipv4();
    if !same_family {
        return Err(format!("range mixes IPv4 and IPv6: {start}-{end}"));
    }
    if s > e {
        return Err(format!("range start {s} > end {e}"));
    }
    Ok(BlacklistEntry::Range { start: s, end: e })
}

/// Parse one cleaned blacklist token: a range (`a-b`), a single IP, or a CIDR.
fn parse_blacklist_entry(line: &str) -> std::result::Result<BlacklistEntry, String> {
    let parts: Vec<&str> = line.split('-').map(str::trim).collect();
    match parts.as_slice() {
        // exactly two `-`-separated parts -> range
        [start, end] => parse_range(start, end),
        // 3+ parts (e.g. a typo'd `a-b-c`) -> explicit error, not a silent fallthrough
        [_, _, _, ..] => Err(format!("invalid range '{line}' (expected exactly one '-')")),
        // no `-` -> single IP / CIDR
        _ => parse_client_ip(line).map(BlacklistEntry::Net),
    }
}

/// Parse blacklist file content: one entry per line. Each line is a single IP,
/// a CIDR, or an inclusive range `start-end`. `#` introduces comments (full-line
/// or trailing), blank lines are ignored, and invalid entries are skipped with a
/// warning so a single bad line never disables the blacklist.
fn parse_blacklist(content: &str) -> Vec<BlacklistEntry> {
    content
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_blacklist_entry(line) {
            Ok(e) => Some(e),
            Err(err) => {
                log::warn!("blacklist: skipping invalid entry '{line}': {err}");
                None
            }
        })
        .collect()
}

/// Read and parse a blacklist file. Errors only on file-read failure.
pub(crate) fn load_blacklist(path: &str) -> std::result::Result<Blacklist, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("failed to read blacklist {path}: {e}"))?;
    Ok(Blacklist::from_entries(parse_blacklist(&content)))
}

/// Load the blacklist for a path that may be `None`. A missing/unreadable file
/// is logged and treated as an empty list — availability over strictness, so a
/// transient read error never blocks all traffic.
pub(crate) fn load_blacklist_state(path: Option<&str>) -> Blacklist {
    match path {
        Some(p) => match load_blacklist(p) {
            Ok(v) => v,
            Err(e) => {
                log::error!("{e}; treating blacklist as empty");
                Blacklist::default()
            }
        },
        None => Blacklist::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_blacklist_handles_comments_blanks_invalid() {
        let content = "\
# full-line comment
10.0.5.100

192.168.66.0/24
   # indented comment
not-an-ip
1.2.3.0/24   # trailing comment
";
        let nets = parse_blacklist(content);
        assert_eq!(nets.len(), 3, "3 valid entries; invalid line skipped");
        let bl = Blacklist::from_entries(nets);
        let contains = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            bl.contains(ip)
        };
        assert!(contains("10.0.5.100"));
        assert!(contains("192.168.66.42"));
        assert!(contains("1.2.3.9"));
    }

    #[test]
    fn parse_blacklist_empty_or_comments_only() {
        assert!(parse_blacklist("").is_empty());
        assert!(parse_blacklist("# only comments\n\n   \n").is_empty());
    }

    #[test]
    fn load_blacklist_reads_file() {
        let path = format!(
            "{}/lb_bl_test_{}.txt",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        std::fs::write(&path, "# header\n10.0.0.9\n10.0.0.0/24\n").unwrap();
        let bl = load_blacklist(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bl.len(), 2);
        let nine: IpAddr = "10.0.0.9".parse().unwrap();
        assert!(bl.contains(nine));
        assert!(bl.contains("10.0.0.55".parse().unwrap()));
    }

    #[test]
    fn load_blacklist_missing_file_errors() {
        assert!(load_blacklist("/nonexistent/lb_bl_missing.txt").is_err());
    }

    #[test]
    fn is_blacklisted_matches_single_ip_and_cidr() {
        let bl = Blacklist::from_entries(vec![
            BlacklistEntry::Net(parse_client_ip("10.0.5.100").unwrap()),
            BlacklistEntry::Net(parse_client_ip("192.168.66.0/24").unwrap()),
        ]);
        // exact single-IP hit
        assert!(bl.contains("10.0.5.100".parse().unwrap()));
        // inside CIDR
        assert!(bl.contains("192.168.66.42".parse().unwrap()));
        // outside everything
        assert!(!bl.contains("10.0.5.99".parse().unwrap()));
        assert!(!bl.contains("192.168.99.1".parse().unwrap()));
        // empty list never blocks
        assert!(!Blacklist::default().contains("10.0.5.100".parse().unwrap()));
    }

    #[test]
    fn parse_blacklist_handles_ranges() {
        let content = "\
10.123.181.128-10.123.181.255
# comment
192.168.0.0-192.168.0.10
";
        let entries = parse_blacklist(content);
        assert_eq!(entries.len(), 2);
        let bl = Blacklist::from_entries(entries);
        let in_list = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            bl.contains(ip)
        };
        // first range: both boundaries and inside
        assert!(in_list("10.123.181.128"));
        assert!(in_list("10.123.181.200"));
        assert!(in_list("10.123.181.255"));
        assert!(!in_list("10.123.181.127"));
        assert!(!in_list("10.123.182.1"));
        // second range
        assert!(in_list("192.168.0.5"));
        assert!(!in_list("192.168.0.11"));
        // an IPv6 query never matches an IPv4 range
        assert!(!in_list("::1"));
    }

    #[test]
    fn parse_blacklist_rejects_bad_ranges() {
        let content = "\
10.0.0.5-10.0.0.1
1.2.3.4-foo
1.2.3.4-::1
10.0.0.1-10.0.0.2-3
";
        let entries = parse_blacklist(content);
        assert!(entries.is_empty(), "all four are invalid ranges");
    }
}
