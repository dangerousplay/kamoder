//! Which address goes into the XAddrs, and the camera's identity numbers.
//!
//! SOAP responses build their URLs from the local address of the TCP
//! connection, so they are always right. WS-Discovery has no connection to
//! look at: the address in ProbeMatch/Hello has to be known up front, and on
//! this camera it is not stable — Wi-Fi can move to another network (and
//! `wifi` can roll back to the previous one), DHCP can hand out a new
//! lease. So the address is re-detected periodically (see `announce.rs`).

use std::net::Ipv4Addr;

/// How the advertised address is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressSource {
    /// `--address`: used as is, never re-detected.
    Fixed(Ipv4Addr),
    /// `--interface`: the first IPv4 address of that interface.
    Interface(String),
    /// Default: the first non-loopback IPv4 address of any interface that is up.
    Auto,
}

impl AddressSource {
    /// The current address, or `None` if there is none (yet).
    pub fn detect(&self) -> Option<Ipv4Addr> {
        match self {
            AddressSource::Fixed(address) => Some(*address),
            AddressSource::Interface(name) => first_ipv4(interfaces(), Some(name)),
            AddressSource::Auto => first_ipv4(interfaces(), None),
        }
    }
}

/// One interface address, reduced to what the selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfAddr {
    pub name: String,
    pub address: Ipv4Addr,
    pub up: bool,
}

fn interfaces() -> Vec<IfAddr> {
    use nix::net::if_::InterfaceFlags;
    let Ok(addresses) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    addresses
        .filter_map(|entry| {
            let address = entry.address?.as_sockaddr_in()?.ip();
            Some(IfAddr {
                name: entry.interface_name,
                address,
                up: entry.flags.contains(InterfaceFlags::IFF_UP),
            })
        })
        .collect()
}

/// Picks the advertised address: on `only` if given, otherwise the first
/// non-loopback, non-link-local address of an interface that is up.
pub fn first_ipv4(addresses: Vec<IfAddr>, only: Option<&str>) -> Option<Ipv4Addr> {
    addresses
        .into_iter()
        .filter(|a| a.up)
        .filter(|a| match only {
            Some(name) => a.name == name,
            None => !a.address.is_loopback() && !a.address.is_link_local(),
        })
        .map(|a| a.address)
        .next()
}

/// MAC address of the first of `names` that has a real one, as twelve
/// lowercase hex digits.
pub fn mac_address(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        let text = std::fs::read_to_string(format!("/sys/class/net/{name}/address")).ok()?;
        normalize_mac(&text)
    })
}

fn normalize_mac(text: &str) -> Option<String> {
    let hex: String = text.trim().chars().filter(|c| *c != ':').collect();
    let valid = hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit());
    (valid && hex.chars().any(|c| c != '0')).then(|| hex.to_ascii_lowercase())
}

/// The SoC name from the `Hardware` line of `/proc/cpuinfo`, e.g. `AK3918AV100`.
pub fn cpuinfo_hardware(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        (key.trim() == "Hardware" && !value.is_empty()).then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str, address: [u8; 4], up: bool) -> IfAddr {
        IfAddr {
            name: name.into(),
            address: address.into(),
            up,
        }
    }

    #[test]
    fn auto_skips_loopback_link_local_and_down_interfaces() {
        let list = vec![
            addr("lo", [127, 0, 0, 1], true),
            addr("eth0", [192, 168, 1, 5], false),
            addr("usb0", [169, 254, 3, 4], true),
            addr("wlan0", [192, 168, 20, 187], true),
        ];
        assert_eq!(
            first_ipv4(list, None),
            Some(Ipv4Addr::new(192, 168, 20, 187))
        );
        assert_eq!(
            first_ipv4(vec![addr("lo", [127, 0, 0, 1], true)], None),
            None
        );
    }

    #[test]
    fn named_interface_is_honoured() {
        let list = vec![
            addr("eth0", [10, 0, 2, 15], true),
            addr("wlan0", [192, 168, 20, 187], true),
        ];
        assert_eq!(
            first_ipv4(list.clone(), Some("wlan0")),
            Some(Ipv4Addr::new(192, 168, 20, 187))
        );
        assert_eq!(first_ipv4(list, Some("wlan1")), None);
    }

    #[test]
    fn fixed_address_is_returned_as_is() {
        let fixed = AddressSource::Fixed(Ipv4Addr::new(192, 0, 2, 10));
        assert_eq!(fixed.detect(), Some(Ipv4Addr::new(192, 0, 2, 10)));
    }

    #[test]
    fn mac_normalisation() {
        assert_eq!(
            normalize_mac("00:1A:2b:3c:4d:5e\n").as_deref(),
            Some("001a2b3c4d5e")
        );
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None);
        assert_eq!(normalize_mac("garbage"), None);
    }

    #[test]
    fn cpuinfo_hardware_line() {
        let cpuinfo = "Processor\t: ARM926EJ-S rev 5 (v5l)\nBogoMIPS\t: 199.47\n\
                       Features\t: swp half thumb fastmult edsp java\n\
                       Hardware\t: AK3918AV100\nRevision\t: 0000\n";
        assert_eq!(cpuinfo_hardware(cpuinfo).as_deref(), Some("AK3918AV100"));
        assert_eq!(cpuinfo_hardware("processor : 0\n"), None);
    }
}
