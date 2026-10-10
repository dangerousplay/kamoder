//! Interface address, default gateway, and whether the gateway answers.
//!
//! "Associated" is not "reachable": a camera can hold a lease on a network
//! whose router is gone, or sit on the right SSID with no DHCP. The gateway
//! check is what separates those from a working link.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// IPv4 address of `iface`, via `SIOCGIFADDR`. `None` when it has none.
pub fn ipv4_address(iface: &str) -> Option<Ipv4Addr> {
    let name = iface.as_bytes();
    // SAFETY: `ifreq` is plain old data; all-zero is a valid value.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    if name.len() >= request.ifr_name.len() {
        return None;
    }
    for (dst, &src) in request.ifr_name.iter_mut().zip(name) {
        *dst = src as libc::c_char;
    }

    // SAFETY: plain socket creation; the fd is closed below on every path.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `request` is a valid, NUL-terminated `ifreq` that outlives the
    // call; SIOCGIFADDR only writes inside it.
    let ret = unsafe { libc::ioctl(fd, libc::SIOCGIFADDR as _, &mut request) };
    // SAFETY: `fd` is ours and closed exactly once.
    unsafe { libc::close(fd) };
    if ret < 0 {
        return None;
    }

    // SAFETY: on success the union holds a `sockaddr`, and for AF_INET that is
    // a `sockaddr_in`, which has the same size.
    let addr: libc::sockaddr_in = unsafe {
        std::ptr::read_unaligned(
            std::ptr::addr_of!(request.ifr_ifru.ifru_addr).cast::<libc::sockaddr_in>(),
        )
    };
    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    (!ip.is_unspecified()).then_some(ip)
}

/// The default gateway through `iface`, from `/proc/net/route`.
pub fn default_gateway(iface: &str) -> Option<Ipv4Addr> {
    parse_route(&std::fs::read_to_string("/proc/net/route").ok()?, iface)
}

/// `/proc/net/route` stores addresses as host-order hex of the network-order
/// bytes, i.e. little-endian on ARM: `0114A8C0` is 192.168.20.1.
pub fn parse_route(table: &str, iface: &str) -> Option<Ipv4Addr> {
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[0] != iface || fields[1] != "00000000" {
            continue;
        }
        let raw = u32::from_str_radix(fields[2], 16).ok()?;
        let gateway = Ipv4Addr::from(raw.to_le_bytes());
        if !gateway.is_unspecified() {
            return Some(gateway);
        }
    }
    None
}

/// Whether `gateway` answers ARP on `iface`.
///
/// Sends one UDP datagram to the discard port to make the kernel resolve the
/// gateway's MAC, then reads the ARP table. No raw socket, no ICMP, and no
/// dependence on the router answering anything above layer 2.
pub fn gateway_reachable(iface: &str, gateway: Ipv4Addr) -> bool {
    if let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
        let _ = socket.send_to(b"", SocketAddrV4::new(gateway, 9));
    }
    // Resolution takes one round trip; the kernel retries for about 3 s.
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(table) = std::fs::read_to_string("/proc/net/arp") {
            if arp_complete(&table, iface, gateway) {
                return true;
            }
        }
    }
    false
}

/// Whether the ARP table holds a resolved entry (`ATF_COM`, flag 0x2) for
/// `ip` on `iface`.
pub fn arp_complete(table: &str, iface: &str, ip: Ipv4Addr) -> bool {
    let ip = ip.to_string();
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() >= 6
            && fields[0] == ip
            && fields[5] == iface
            && u32::from_str_radix(fields[2].trim_start_matches("0x"), 16)
                .is_ok_and(|flags| flags & 0x2 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
wlan0\t00000000\t0114A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
wlan0\t0014A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";

    #[test]
    fn default_route_is_decoded() {
        assert_eq!(
            parse_route(ROUTE, "wlan0"),
            Some(Ipv4Addr::new(192, 168, 20, 1))
        );
    }

    #[test]
    fn other_interfaces_and_missing_routes() {
        assert_eq!(parse_route(ROUTE, "eth0"), None);
        assert_eq!(parse_route("Iface\tDestination\tGateway\n", "wlan0"), None);
    }

    const ARP: &str =
        "IP address       HW type     Flags       HW address            Mask     Device\n\
192.168.20.1     0x1         0x2         aa:bb:cc:dd:ee:ff     *        wlan0\n\
192.168.20.9     0x1         0x0         00:00:00:00:00:00     *        wlan0\n";

    #[test]
    fn arp_entry_must_be_complete_and_on_the_interface() {
        assert!(arp_complete(ARP, "wlan0", Ipv4Addr::new(192, 168, 20, 1)));
        assert!(!arp_complete(ARP, "wlan0", Ipv4Addr::new(192, 168, 20, 9)));
        assert!(!arp_complete(ARP, "eth0", Ipv4Addr::new(192, 168, 20, 1)));
        assert!(!arp_complete(ARP, "wlan0", Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn loopback_has_an_address() {
        assert_eq!(ipv4_address("lo"), Some(Ipv4Addr::LOCALHOST));
        assert_eq!(ipv4_address("definitely-not-an-iface"), None);
    }
}
