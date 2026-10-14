//! One SNTP exchange ([`query`]) and one sync round across the configured
//! servers ([`sync`], NTP-02/04).
//!
//! - **DNS every round**: names are resolved again on each round, never
//!   cached here, and the resolver is told to re-read `/etc/resolv.conf`
//!   first. uClibc reads that file once per process otherwise, and at boot
//!   this service may start before DHCP has written it.
//! - **One socket per exchange**, `connect`ed to the server, so the kernel
//!   drops datagrams from anyone else and an ICMP port-unreachable surfaces as
//!   `ECONNREFUSED` straight away instead of a 2 s wait.
//! - **Random originate cookie**: the request's transmit field carries 64
//!   random bits, not the local time, so an off-path spoofer cannot guess the
//!   value a reply must echo, and the request does not reveal the local clock.
//!   T1 is kept locally.

use crate::clock::Clock;
use crate::packet::{self, Packet, Reject, Timestamp};
use snafu::{ResultExt, Snafu};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// The NTP port, used when a server is given without one.
pub const PORT: u16 = 123;
/// How long to wait for each reply (NTP-02).
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// Addresses tried per server name per round: pool names resolve to four,
/// and four 2 s timeouts per name would make a round with the network down
/// needlessly long.
pub const ADDRS_PER_SERVER: usize = 2;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot resolve {server}: {source}"))]
    Resolve { server: String, source: io::Error },
    #[snafu(display("{server} resolves to no address"))]
    NoAddress { server: String },
    #[snafu(display("cannot open a UDP socket to {addr}: {source}"))]
    Socket { addr: SocketAddr, source: io::Error },
    #[snafu(display("cannot send to {addr}: {source}"))]
    Send { addr: SocketAddr, source: io::Error },
    #[snafu(display("no reply from {addr} within {timeout:?}"))]
    Timeout { addr: SocketAddr, timeout: Duration },
    #[snafu(display("receiving from {addr}: {source}"))]
    Receive { addr: SocketAddr, source: io::Error },
    #[snafu(display("bad reply from {addr}: {source}"))]
    Decode {
        addr: SocketAddr,
        source: packet::DecodeError,
    },
    #[snafu(display("reply from {addr} rejected: {source}"))]
    Rejected { addr: SocketAddr, source: Reject },
    #[snafu(display("none of {count} servers gave a usable reply"))]
    AllFailed { count: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What one exchange measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Server time minus local time, nanoseconds.
    pub offset: i64,
    /// Round-trip delay, nanoseconds.
    pub delay: i64,
    pub stratum: u8,
}

/// A successful round: which server answered and what it said.
#[derive(Debug, Clone)]
pub struct Synced {
    pub server: String,
    pub addr: SocketAddr,
    pub sample: Sample,
}

/// The standard four-timestamp formula (RFC 4330 §5), in nanoseconds:
/// returns `(offset, delay)`. Done in `i128` so a 1970 local clock against a
/// 2026 server cannot overflow.
pub fn compute(t1: i64, t2: i64, t3: i64, t4: i64) -> (i64, i64) {
    let (t1, t2, t3, t4) = (t1 as i128, t2 as i128, t3 as i128, t4 as i128);
    let offset = ((t2 - t1) + (t3 - t4)) / 2;
    let delay = (t4 - t1) - (t3 - t2);
    (offset as i64, delay as i64)
}

/// 64 unpredictable bits for the originate cookie. std's `RandomState` is
/// seeded from the OS once per thread and advanced per instance, which is
/// plenty against off-path guessing, without a `rand` dependency.
fn cookie() -> Timestamp {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0x6b61_6d6f_6465_7221);
    Timestamp::from_bits(hasher.finish() | 1)
}

/// One request/reply with `addr`, validated against `floor_unix`.
pub fn query(
    addr: SocketAddr,
    timeout: Duration,
    clock: &dyn Clock,
    floor_unix: i64,
) -> Result<Sample> {
    let local: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(local).context(SocketSnafu { addr })?;
    socket.connect(addr).context(SocketSnafu { addr })?;
    socket
        .set_read_timeout(Some(timeout))
        .context(SocketSnafu { addr })?;

    let cookie = cookie();
    let request = Packet::request(cookie).encode();
    let t1 = clock.now();
    socket.send(&request).context(SendSnafu { addr })?;
    let mut buf = [0u8; 128];
    let len = match socket.recv(&mut buf) {
        Ok(len) => len,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            return TimeoutSnafu { addr, timeout }.fail();
        }
        Err(source) => return Err(Error::Receive { addr, source }),
    };
    let t4 = clock.now();

    let reply = Packet::decode(&buf[..len]).context(DecodeSnafu { addr })?;
    reply
        .validate(cookie, floor_unix)
        .context(RejectedSnafu { addr })?;
    let (offset, delay) = compute(
        t1,
        reply.receive.to_unix_nanos(),
        reply.transmit.to_unix_nanos(),
        t4,
    );
    Ok(Sample {
        offset,
        delay,
        stratum: reply.stratum,
    })
}

/// Splits `host`, `host:port`, `ip:port` or `[v6]:port` into a host and a
/// port, defaulting to [`PORT`]. A bare IPv6 address keeps its colons.
fn host_port(server: &str) -> (&str, u16) {
    if let Some(rest) = server.strip_prefix('[') {
        if let Some((host, port)) = rest.split_once("]:") {
            if let Ok(port) = port.parse() {
                return (host, port);
            }
        }
        return (rest.trim_end_matches(']'), PORT);
    }
    match server.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, port),
            Err(_) => (server, PORT),
        },
        _ => (server, PORT),
    }
}

/// Resolves `server` now (NTP-04: DNS on every attempt).
pub fn resolve(server: &str) -> Result<Vec<SocketAddr>> {
    let (host, port) = host_port(server);
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .context(ResolveSnafu { server })?
        .collect();
    if addrs.is_empty() {
        return NoAddressSnafu { server }.fail();
    }
    Ok(ipv4_first(addrs))
}

/// IPv4 before IPv6, otherwise in resolver order: camera kernels are often
/// built without IPv6, and an AAAA answer tried first just burns one of the
/// [`ADDRS_PER_SERVER`] attempts on `EAFNOSUPPORT`.
fn ipv4_first(mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    addrs.sort_by_key(SocketAddr::is_ipv6);
    addrs
}

/// Makes the resolver re-read `/etc/resolv.conf`; see the module docs.
fn reload_resolver() {
    // SAFETY: res_init takes no arguments and only rewrites the resolver's
    // global state; this process resolves names from this thread only.
    unsafe {
        libc::res_init();
    }
}

/// One round: each server in order, up to [`ADDRS_PER_SERVER`] addresses
/// each, until one gives a valid reply (NTP-02). Every failure is logged.
pub fn sync(
    servers: &[String],
    timeout: Duration,
    clock: &dyn Clock,
    floor_unix: i64,
) -> Result<Synced> {
    reload_resolver();
    for server in servers {
        let addrs = match resolve(server) {
            Ok(addrs) => addrs,
            Err(error) => {
                tracing::warn!(%server, %error, "server skipped");
                continue;
            }
        };
        for addr in addrs.into_iter().take(ADDRS_PER_SERVER) {
            match query(addr, timeout, clock, floor_unix) {
                Ok(sample) => {
                    return Ok(Synced {
                        server: server.clone(),
                        addr,
                        sample,
                    })
                }
                Err(error) => tracing::warn!(%server, %error, "no time from this server"),
            }
        }
    }
    AllFailedSnafu {
        count: servers.len(),
    }
    .fail()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::fake::FakeClock;
    use crate::packet::{MODE_SERVER, NANOS};
    use std::thread::JoinHandle;

    const FLOOR: i64 = crate::BUILD_FLOOR_UNIX;
    /// What the fake servers claim the time is.
    const SERVER_NOW: i64 = (FLOOR + 3_600) * NANOS;

    #[test]
    fn ipv4_addresses_are_tried_first() {
        let v6: SocketAddr = "[2001:db8::1]:123".parse().unwrap();
        let a: SocketAddr = "192.0.2.1:123".parse().unwrap();
        let b: SocketAddr = "192.0.2.2:123".parse().unwrap();
        assert_eq!(ipv4_first(vec![v6, a, b]), vec![a, b, v6]);
    }

    #[test]
    fn four_timestamp_formula() {
        // Client 1 s behind, 40 ms each way, 2 ms in the server.
        let t1 = 100 * NANOS;
        let t2 = 101 * NANOS + 40_000_000;
        let t3 = t2 + 2_000_000;
        let t4 = t1 + 82_000_000;
        assert_eq!(compute(t1, t2, t3, t4), (NANOS, 80_000_000));
    }

    #[test]
    fn formula_with_asymmetric_path_and_ahead_clock() {
        // Client 5 s ahead; 10 ms out, 30 ms back: offset is off by half the
        // asymmetry, as the formula implies.
        let t1 = 1_000 * NANOS;
        let t2 = t1 - 5 * NANOS + 10_000_000;
        let t3 = t2;
        let t4 = t1 + 40_000_000;
        assert_eq!(
            compute(t1, t2, t3, t4),
            (-5 * NANOS - 10_000_000, 40_000_000)
        );
    }

    #[test]
    fn formula_does_not_overflow_from_1970() {
        let (offset, delay) = compute(0, SERVER_NOW, SERVER_NOW, 0);
        assert_eq!((offset, delay), (SERVER_NOW, 0));
    }

    #[test]
    fn host_port_forms() {
        assert_eq!(host_port("pool.ntp.org"), ("pool.ntp.org", 123));
        assert_eq!(host_port("127.0.0.1:1234"), ("127.0.0.1", 1234));
        assert_eq!(host_port("[::1]:1234"), ("::1", 1234));
        assert_eq!(host_port("[::1]"), ("::1", 123));
        assert_eq!(host_port("::1"), ("::1", 123));
        assert_eq!(host_port("host:notaport"), ("host:notaport", 123));
    }

    #[test]
    fn cookies_differ() {
        assert_ne!(cookie(), cookie());
    }

    /// A one-shot SNTP server on 127.0.0.1: answers the first request with
    /// whatever `respond` makes of it, then exits.
    fn fake_server(
        respond: impl FnOnce(Packet) -> Packet + Send + 'static,
    ) -> (String, JoinHandle<()>) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut buf = [0u8; 128];
            let (len, peer) = socket.recv_from(&mut buf).unwrap();
            let request = Packet::decode(&buf[..len]).unwrap();
            assert_eq!(request.mode, crate::packet::MODE_CLIENT);
            socket.send_to(&respond(request).encode(), peer).unwrap();
        });
        (addr.to_string(), handle)
    }

    fn honest(request: Packet) -> Packet {
        Packet {
            leap: 0,
            version: 4,
            mode: MODE_SERVER,
            stratum: 2,
            originate: request.transmit,
            receive: Timestamp::from_unix_nanos(SERVER_NOW),
            transmit: Timestamp::from_unix_nanos(SERVER_NOW),
            ..Packet::default()
        }
    }

    /// A port nobody listens on: bound, then closed again.
    fn dead_server() -> String {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().to_string()
    }

    #[test]
    fn query_measures_offset_against_fake_server() {
        let (server, handle) = fake_server(honest);
        let clock = FakeClock::at(0);
        let addr = server.parse().unwrap();
        let sample = query(addr, TIMEOUT, &clock, FLOOR).unwrap();
        handle.join().unwrap();
        // The fake clock does not move, so T1 = T4 = 0.
        assert_eq!(
            sample,
            Sample {
                offset: SERVER_NOW,
                delay: 0,
                stratum: 2
            }
        );
    }

    #[test]
    fn query_rejects_wrong_originate() {
        let (server, handle) = fake_server(|request| Packet {
            originate: Timestamp::from_bits(request.transmit.to_bits() ^ 1),
            ..honest(request)
        });
        let clock = FakeClock::at(0);
        let error = query(server.parse().unwrap(), TIMEOUT, &clock, FLOOR).unwrap_err();
        handle.join().unwrap();
        assert!(
            matches!(
                error,
                Error::Rejected {
                    source: Reject::Originate,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn query_times_out_on_silence() {
        // Bound and never read: the request is swallowed.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let clock = FakeClock::at(0);
        let timeout = Duration::from_millis(200);
        let error = query(silent.local_addr().unwrap(), timeout, &clock, FLOOR).unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error}");
    }

    #[test]
    fn sync_falls_back_to_the_next_server() {
        let (bad, bad_handle) = fake_server(|request| Packet {
            stratum: 16,
            ..honest(request)
        });
        let (good, good_handle) = fake_server(honest);
        let servers = vec![dead_server(), bad, good.clone()];
        let clock = FakeClock::at(0);
        let synced = sync(&servers, Duration::from_millis(500), &clock, FLOOR).unwrap();
        bad_handle.join().unwrap();
        good_handle.join().unwrap();
        assert_eq!(synced.server, good);
        assert_eq!(synced.sample.offset, SERVER_NOW);
        assert!(
            clock.log.borrow().is_empty(),
            "sync itself never touches the clock"
        );
    }

    #[test]
    fn sync_reports_all_failed() {
        let servers = vec![dead_server(), "host.invalid".to_owned()];
        let clock = FakeClock::at(0);
        let error = sync(&servers, Duration::from_millis(200), &clock, FLOOR).unwrap_err();
        assert!(matches!(error, Error::AllFailed { count: 2 }), "{error}");
    }
}
