//! WS-Discovery Hello/Bye announcement lifecycle tests (parity with
//! onvif-go's discovery.Responder): `start()` multicasts a Hello to the
//! WS-Discovery group, stopping multicasts a Bye.
//!
//! These tests join a real multicast group (239.255.255.250:3702).
//! Environments without multicast support (some CI sandboxes, containers
//! without a multicast route) skip via the socket-setup guard — setup
//! failing means multicast is unavailable, not that the announcement is
//! broken.

use std::net::Ipv4Addr;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use onvif_device_rs::discovery::{detect_local_ip, DiscoveryServer, BYE_ACTION, HELLO_ACTION};

/// The WS-Discovery multicast group.
const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// Bind a receiver on `:3702` and join the WS-Discovery group on the same
/// interface the responder will use. Returns `None` when the environment
/// cannot do multicast — the caller skips.
///
/// The receiver must exist (and be joined) *before* the responder starts:
/// Hello is announced exactly once at start.
async fn multicast_receiver() -> Option<UdpSocket> {
    let iface: Ipv4Addr = detect_local_ip().parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
    let bind_addr = std::net::SocketAddr::from((Ipv4Addr::UNSPECIFIED, 3702));

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    sock.set_reuse_address(true).ok()?;
    sock.set_nonblocking(true).ok()?;
    sock.bind(&bind_addr.into()).ok()?;
    let std_sock: std::net::UdpSocket = sock.into();
    std_sock.join_multicast_v4(&GROUP, &iface).ok()?;
    UdpSocket::from_std(std_sock).ok()
}

/// Receive datagrams until one mentioning `uuid` **and** `action` arrives
/// (other hosts on the LAN legitimately multicast to the same group, and
/// our own Hello/Bye pair may arrive in either order relative to the
/// drop) or the deadline passes.
async fn recv_announcement(
    rx: &UdpSocket,
    uuid: &str,
    action: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            return None;
        }
        let mut buf = vec![0u8; 8192];
        let (n, _src) = tokio::time::timeout(remain, rx.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if text.contains(uuid) && text.contains(action) {
            return Some(text);
        }
    }
}

/// `start()` multicasts Hello (Action `.../Hello`, `<d:Hello>` body with
/// the device announcement); `shutdown()` multicasts Bye (Action `.../Bye`
/// with the EndpointReference only).
#[tokio::test]
async fn start_announces_hello_and_stop_announces_bye() {
    let Some(rx) = multicast_receiver().await else {
        eprintln!("skipping: this environment has no multicast support");
        return;
    };

    let server = DiscoveryServer::with_identity("", 8080, "Parity Cam", "HW-PARITY");
    let uuid = server.uuid().to_string();
    let mut handle = server.start().await.expect("responder starts");

    let hello = recv_announcement(&rx, &uuid, HELLO_ACTION, Duration::from_secs(5))
        .await
        .expect("Hello announcement within 5s");
    assert!(hello.contains(HELLO_ACTION), "hello action: {hello}");
    assert!(hello.contains("<d:Hello"), "hello body: {hello}");
    assert!(
        hello.contains("onvif://www.onvif.org/hardware/HW-PARITY"),
        "hello scopes: {hello}"
    );
    assert!(
        hello.contains("<d:XAddrs>http://"),
        "hello advertises reachability: {hello}"
    );

    handle.shutdown().await.expect("clean shutdown");

    let bye = recv_announcement(&rx, &uuid, BYE_ACTION, Duration::from_secs(5))
        .await
        .expect("Bye announcement within 5s");
    assert!(bye.contains(BYE_ACTION), "bye action: {bye}");
    assert!(bye.contains("<d:Bye"), "bye body: {bye}");
    assert!(!bye.contains("<d:XAddrs>"), "bye carries no XAddrs: {bye}");
}

/// Dropping the handle (no explicit `shutdown()`) must still announce Bye
/// — the doc contract says dropping stops the responder.
#[tokio::test]
async fn dropping_handle_announces_bye() {
    let Some(rx) = multicast_receiver().await else {
        eprintln!("skipping: this environment has no multicast support");
        return;
    };

    let server = DiscoveryServer::new("", 8080);
    let uuid = server.uuid().to_string();
    let handle = server.start().await.expect("responder starts");
    drop(handle);

    let bye = recv_announcement(&rx, &uuid, BYE_ACTION, Duration::from_secs(5))
        .await
        .expect("Bye announcement within 5s of drop");
    assert!(bye.contains(BYE_ACTION), "bye action: {bye}");
}
