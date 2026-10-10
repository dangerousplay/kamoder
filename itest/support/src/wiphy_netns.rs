//! `wiphy-netns <iface> <pid>` — moves a wireless PHY into another process's
//! network namespace.
//!
//! The integration tests simulate the camera's radio and two access points
//! with `mac80211_hwsim`, all in one VM. If the access points stayed in the
//! camera's namespace, their gateway addresses would be *local* to the camera
//! and traffic to them would go over loopback instead of the simulated air —
//! the gateway check would pass without testing anything. Each AP therefore
//! runs in its own namespace, like a separate machine.
//!
//! Wireless netdevs cannot be moved with `ip link set netns`; the whole wiphy
//! has to move, through nl80211's `NL80211_CMD_SET_WIPHY_NETNS`. The camera
//! has no `iw`, so this sends that one generic-netlink command itself.

extern crate os;

use std::io::{Error, ErrorKind, Result};

const NETLINK_GENERIC: i32 = 16;
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NL80211_CMD_SET_WIPHY_NETNS: u8 = 49;
const NL80211_ATTR_WIPHY: u16 = 1;
const NL80211_ATTR_PID: u16 = 82;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLMSG_ERROR: u16 = 2;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Builds `nlmsghdr + genlmsghdr + attributes`.
fn message(kind: u16, flags: u16, seq: u32, cmd: u8, attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = vec![cmd, 1, 0, 0]; // genlmsghdr: cmd, version, reserved
    for (kind, payload) in attrs {
        let len = 4 + payload.len();
        body.extend_from_slice(&(len as u16).to_ne_bytes());
        body.extend_from_slice(&kind.to_ne_bytes());
        body.extend_from_slice(payload);
        body.resize(align4(body.len()), 0);
    }
    let mut msg = Vec::with_capacity(16 + body.len());
    msg.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(&flags.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes()); // port id: kernel assigns
    msg.extend_from_slice(&body);
    msg
}

struct Socket(i32);

impl Socket {
    fn open() -> Result<Self> {
        // SAFETY: plain socket creation; closed in Drop.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_GENERIC) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(Self(fd))
    }

    fn send(&self, msg: &[u8]) -> Result<()> {
        // SAFETY: zeroed sockaddr_nl addresses the kernel (pid 0).
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: `msg` and `addr` are valid for the duration of the call.
        let ret = unsafe {
            libc::sendto(
                self.0,
                msg.as_ptr().cast(),
                msg.len(),
                0,
                std::ptr::addr_of!(addr).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn recv(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 8192];
        // SAFETY: `buf` is writable for its full length.
        let ret = unsafe { libc::recv(self.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if ret < 0 {
            return Err(Error::last_os_error());
        }
        buf.truncate(ret as usize);
        Ok(buf)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: our fd, closed once.
        unsafe { libc::close(self.0) };
    }
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([buf[at], buf[at + 1]])
}

/// Resolves the dynamic generic-netlink family id of nl80211.
fn nl80211_family(socket: &Socket) -> Result<u16> {
    socket.send(&message(
        GENL_ID_CTRL,
        NLM_F_REQUEST,
        1,
        CTRL_CMD_GETFAMILY,
        &[(CTRL_ATTR_FAMILY_NAME, b"nl80211\0".to_vec())],
    ))?;
    let reply = socket.recv()?;
    if reply.len() < 20 || u16_at(&reply, 4) == NLMSG_ERROR {
        return Err(Error::new(
            ErrorKind::NotFound,
            "nl80211 family not found (no cfg80211?)",
        ));
    }
    // Attributes start after nlmsghdr (16) + genlmsghdr (4).
    let mut at = 20;
    while at + 4 <= reply.len() {
        let len = u16_at(&reply, at) as usize;
        if len < 4 {
            break;
        }
        if u16_at(&reply, at + 2) == CTRL_ATTR_FAMILY_ID {
            return Ok(u16_at(&reply, at + 4));
        }
        at += align4(len);
    }
    Err(Error::new(ErrorKind::InvalidData, "no family id in reply"))
}

fn move_wiphy(wiphy: u32, pid: u32) -> Result<()> {
    let socket = Socket::open()?;
    let family = nl80211_family(&socket)?;
    socket.send(&message(
        family,
        NLM_F_REQUEST | NLM_F_ACK,
        2,
        NL80211_CMD_SET_WIPHY_NETNS,
        &[
            (NL80211_ATTR_WIPHY, wiphy.to_ne_bytes().to_vec()),
            (NL80211_ATTR_PID, pid.to_ne_bytes().to_vec()),
        ],
    ))?;
    let ack = socket.recv()?;
    if ack.len() >= 20 && u16_at(&ack, 4) == NLMSG_ERROR {
        let code = i32::from_ne_bytes([ack[16], ack[17], ack[18], ack[19]]);
        if code != 0 {
            return Err(Error::from_raw_os_error(-code));
        }
    }
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() != 3 {
        eprintln!("usage: wiphy-netns <interface> <pid>");
        std::process::exit(2);
    }
    let index_path = format!("/sys/class/net/{}/phy80211/index", argv[1]);
    let wiphy: u32 = match std::fs::read_to_string(&index_path).map(|s| s.trim().parse()) {
        Ok(Ok(index)) => index,
        _ => {
            eprintln!("wiphy-netns: cannot read {index_path}");
            std::process::exit(1);
        }
    };
    let Ok(pid) = argv[2].parse::<u32>() else {
        eprintln!("wiphy-netns: bad pid {}", argv[2]);
        std::process::exit(2);
    };
    if let Err(error) = move_wiphy(wiphy, pid) {
        eprintln!(
            "wiphy-netns: moving phy{wiphy} ({}) to pid {pid}: {error}",
            argv[1]
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_layout_is_aligned_and_sized() {
        let msg = message(
            GENL_ID_CTRL,
            NLM_F_REQUEST,
            7,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_NAME, b"nl80211\0".to_vec())],
        );
        // 16 header + 4 genl + (4 + 8) attribute, already aligned.
        assert_eq!(msg.len(), 32);
        assert_eq!(u32::from_ne_bytes(msg[0..4].try_into().unwrap()), 32);
        assert_eq!(u16_at(&msg, 4), GENL_ID_CTRL);
        assert_eq!(msg[16], CTRL_CMD_GETFAMILY);
        assert_eq!(u16_at(&msg, 20), 12); // nla_len
        assert_eq!(u16_at(&msg, 22), CTRL_ATTR_FAMILY_NAME);
    }

    #[test]
    fn odd_attribute_is_padded() {
        let msg = message(1, 0, 0, 0, &[(9, vec![1, 2, 3])]);
        assert_eq!(msg.len() % 4, 0);
        assert_eq!(msg.len(), 16 + 4 + 8);
    }
}
