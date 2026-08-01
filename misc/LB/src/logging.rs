use log::{Level, LevelFilter, Log, Metadata, Record};
use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic counter for request IDs; combined with wall-clock nanos below it
/// is unique enough per process across threads.
static REQ_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a compact, unique-per-process request ID (hex): `<nanos>-<counter>`.
pub(crate) fn generate_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = REQ_ID_COUNTER.fetch_add(1, Relaxed);
    format!("{nanos:016x}-{counter:08x}")
}

/// Strip control characters from a client-controlled field before it reaches a
/// log line, preventing syslog / terminal log injection. Protocol parsers
/// normally reject literal CR/LF, but this is cheap defense-in-depth.
pub(crate) fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c.is_control()) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect(),
    )
}

pub(crate) fn dispatch_line(
    method: &str,
    host: &str,
    path: &str,
    client: Option<IpAddr>,
    backend: SocketAddr,
) -> String {
    let client = client
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "dispatch method={} host={} path={} client={} backend={backend}",
        sanitize(method),
        sanitize(host),
        sanitize(path),
        sanitize(&client)
    )
}

/// Build the structured "request redirected by the LB" log line. `location`
/// comes from config (trusted), but the request-derived fields are sanitized.
pub(crate) fn redirect_line(code: u16, host: &str, path: &str, location: &str) -> String {
    format!(
        "redirect {code} {}{} -> {}",
        sanitize(host),
        sanitize(path),
        sanitize(location)
    )
}

/// Build the "request completed" line: response status, elapsed ms and body
/// bytes sent. Emitted from the `logging()` hook, adjacent to (and correlated
/// with) the earlier `dispatch` line via the backend address.
pub(crate) fn complete_line(backend: SocketAddr, status: u16, ms: u128, bytes: u64) -> String {
    format!("complete backend={backend} status={status} ms={ms} bytes={bytes}")
}
/// syslog LOG_USER facility code.
const FACILITY_USER: u8 = 1;

/// Map a `log::Level` to a syslog PRI value = facility * 8 + severity.
fn syslog_priority(level: Level) -> u8 {
    let severity = match level {
        Level::Error => 3, // ERR
        Level::Warn => 4,  // WARNING
        Level::Info => 6,  // INFO
        Level::Debug => 7, // DEBUG
        Level::Trace => 7, // (no severity below DEBUG)
    };
    FACILITY_USER * 8 + severity
}

/// Writes log records to a local syslog daemon over a connected Unix datagram
/// socket, in RFC 3164 form: `<PRI>ident[pid]: message`.
struct SyslogLogger {
    sock: UnixDatagram,
    ident: String,
    pid: u32,
}

impl Log for SyslogLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let line = format!(
            "<{}>{}[{}]: {}",
            syslog_priority(record.level()),
            self.ident,
            self.pid,
            record.args()
        );
        // Best-effort: syslog is fire-and-forget, never block the data path.
        let _ = self.sock.send(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Last-resort logger: writes to stderr so records are never silently dropped
/// when no local syslog socket is available (e.g. some containers).
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        eprintln!(
            "{} {}",
            record.level().as_str().to_lowercase(),
            record.args()
        );
    }

    fn flush(&self) {}
}

/// Create a datagram socket connected to a syslog path. The socket is
/// non-blocking so a slow/jammed syslogd can never stall the data path: a full
/// buffer drops the log line (best-effort) instead of blocking the worker.
fn connect_syslog(path: &str) -> Option<UnixDatagram> {
    let sock = UnixDatagram::unbound().ok()?;
    sock.set_nonblocking(true).ok()?;
    sock.connect(path).ok()?;
    Some(sock)
}

/// Install the global `log` backend: prefer a local syslog Unix socket
/// (`/dev/log`, then the macOS/BSD variants), otherwise fall back to stderr.
/// pingora's own `log::` calls route through here too.
pub(crate) fn init_logging(ident: &str) {
    let pid = std::process::id();
    for path in ["/dev/log", "/var/run/syslog", "/var/run/log"] {
        let sock = match connect_syslog(path) {
            Some(s) => s,
            None => continue,
        };
        let logger = SyslogLogger {
            sock,
            ident: ident.to_string(),
            pid,
        };
        if log::set_boxed_logger(Box::new(logger)).is_ok() {
            log::set_max_level(LevelFilter::Info);
            return;
        }
    }
    // No syslog socket reachable — log to stderr instead of dropping records.
    let _ = log::set_boxed_logger(Box::new(StderrLogger));
    log::set_max_level(LevelFilter::Info);
    eprintln!("{ident}: no syslog socket found, logging to stderr");
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_line_contains_all_fields() {
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let backend: SocketAddr = "10.0.1.2:8080".parse().unwrap();
        let line = dispatch_line("POST", "api.lb.local", "/v1/chat", Some(ip), backend);
        assert!(line.contains("method=POST"));
        assert!(line.contains("host=api.lb.local"));
        assert!(line.contains("path=/v1/chat"));
        assert!(line.contains("client=10.0.0.9"));
        assert!(line.contains("backend=10.0.1.2:8080"));

        // Missing client IP is rendered as "-".
        let line2 = dispatch_line("GET", "h", "/p", None, backend);
        assert!(line2.contains("client=-"));
    }

    #[test]
    fn sanitize_strips_control_chars_keeps_plain_text() {
        assert_eq!(sanitize("plain-host.example.com"), "plain-host.example.com");
        // CR, LF and ESC are replaced; printable chars are preserved.
        assert_eq!(sanitize("bad\r\nhost\x1b[31m"), "bad??host?[31m");
        assert_eq!(
            dispatch_line("GET", "a\r\nb", "/x", None, "10.0.0.1:80".parse().unwrap()),
            "dispatch method=GET host=a??b path=/x client=- backend=10.0.0.1:80"
        );
        assert_eq!(
            redirect_line(302, "a\nb", "/x", "/login"),
            "redirect 302 a?b/x -> /login"
        );
    }

    #[test]
    fn complete_line_contains_status_duration_bytes() {
        let line = complete_line("10.0.0.1:80".parse().unwrap(), 200, 12, 4096);
        assert_eq!(
            line,
            "complete backend=10.0.0.1:80 status=200 ms=12 bytes=4096"
        );
    }

    #[test]
    fn request_ids_are_unique_and_hex() {
        let a = generate_request_id();
        let b = generate_request_id();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(b.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn syslog_priority_maps_levels_to_user_facility() {
        use log::Level;
        assert_eq!(syslog_priority(Level::Error), FACILITY_USER * 8 + 3);
        assert_eq!(syslog_priority(Level::Warn), FACILITY_USER * 8 + 4);
        assert_eq!(syslog_priority(Level::Info), FACILITY_USER * 8 + 6);
        assert_eq!(syslog_priority(Level::Debug), FACILITY_USER * 8 + 7);
        assert_eq!(syslog_priority(Level::Trace), FACILITY_USER * 8 + 7);
    }

    #[test]
    fn syslog_logger_emits_rfc3164_to_socket() {
        // Stand up a local datagram sink, point the logger at it, and read back
        // exactly what it emits — a deterministic end-to-end check of the wire
        // format without depending on the host syslogd.
        let sock_path =
            std::env::temp_dir().join(format!("lb_syslog_test_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixDatagram::bind(&sock_path).unwrap();

        let sock = UnixDatagram::unbound().unwrap();
        sock.connect(&sock_path).unwrap();
        let logger = SyslogLogger {
            sock,
            ident: "gateway-lb".to_string(),
            pid: 4242,
        };

        logger.log(
            &Record::builder()
                .args(format_args!("dispatch method=GET backend=10.0.0.1:80"))
                .level(Level::Info)
                .target("gateway-lb")
                .build(),
        );

        let mut buf = [0u8; 256];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        let msg = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            msg,
            "<14>gateway-lb[4242]: dispatch method=GET backend=10.0.0.1:80"
        );
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn connect_syslog_socket_is_nonblocking() {
        // Bind a listener that never reads, so its receive buffer fills. A
        // non-blocking sender must then error (drop) instead of blocking the
        // worker forever under syslog pressure.
        let sock_path =
            std::env::temp_dir().join(format!("lb_syslog_nb_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixDatagram::bind(&sock_path).unwrap();

        let sock = connect_syslog(sock_path.to_str().unwrap())
            .expect("should connect to the bound listener");

        let payload = b"<14>t[0]: padded-log-message-bytes-aaaaaaaaaaaaaaaa";
        let mut errored = false;
        for _ in 0..200_000 {
            if sock.send(payload).is_err() {
                errored = true;
                break;
            }
        }
        assert!(
            errored,
            "non-blocking syslog socket must drop under pressure, not block"
        );
        drop(listener);
        let _ = std::fs::remove_file(&sock_path);
    }
}
