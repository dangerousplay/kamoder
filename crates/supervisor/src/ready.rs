//! Boot timing and the "ready" report on the serial console.
//!
//! At start the banner shows when the kernel booted and when the supervisor
//! started. Once every enabled service has been up for [`SETTLE`] and the
//! camera has an IPv4 address, one report says how long that took and how
//! to reach the camera: Wi-Fi network and signal, addresses, and the ports
//! each service is listening on. If that never happens within
//! [`READY_TIMEOUT`], the same report is printed marked NOT READY, naming
//! what is missing.
//!
//! Everything is read from generic Linux interfaces (`/proc`, `/sys`, the
//! wpa_supplicant control socket), never a vendor SDK. The parsers are pure
//! functions over file contents so they are tested on the host.

use crate::console::Console;
use crate::supervisor::{StatusAll, Supervisor};
use ::supervisor::control::{ServiceStatus, State};
use kameo::actor::ActorRef;
use std::collections::{BTreeSet, HashMap};
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::console::Stamp;

/// A service counts as up once it has run this long without restarting.
const SETTLE: Duration = Duration::from_secs(3);
/// Give up waiting and report what is missing after this long.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(1);

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BLACK_ON_GREEN: &str = "\x1b[1;30;42m";
const WHITE_ON_RED: &str = "\x1b[1;97;41m";

/// When the supervisor started: kernel uptime and, if the clock is set,
/// wall time.
#[derive(Debug, Clone, Copy)]
pub struct StartTime {
    pub uptime: f64,
    pub wall: Option<u64>,
}

impl StartTime {
    pub fn now() -> Self {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
            // No RTC: the camera boots at 1970 until NTP sets the clock.
            .filter(|secs| *secs >= os::time::CLOCK_VALID_FLOOR_UNIX as u64);
        Self {
            uptime: crate::console::uptime(),
            wall,
        }
    }

    /// Banner rows: kernel boot and supervisor start, in local time with
    /// its UTC offset (`TZ` from `init.sh`).
    pub fn banner_rows(&self) -> [(&'static str, String); 2] {
        let boot = match self.wall {
            Some(wall) => format!(
                "{}  {DIM}({:.2} s before start){RESET}",
                local(wall.saturating_sub(self.uptime as u64)),
                self.uptime
            ),
            None => format!(
                "{:.2} s before start  {DIM}(clock not set yet: no RTC){RESET}",
                self.uptime
            ),
        };
        let start = match self.wall {
            Some(wall) => format!("{}  {DIM}(uptime {:.2} s){RESET}", local(wall), self.uptime),
            None => format!("uptime {:.2} s", self.uptime),
        };
        [("Booted", boot), ("Started", start)]
    }
}

/// Where to look for wpa_supplicant's control socket, in order.
pub fn wpa_ctrl_dirs(wifi_args: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = wifi_args
        .windows(2)
        .filter(|pair| pair[0] == "--ctrl-dir")
        .map(|pair| PathBuf::from(&pair[1]))
        .collect();
    dirs.push(PathBuf::from("/etc/Wireless"));
    dirs.push(PathBuf::from("/var/run/wpa_supplicant"));
    dirs
}

/// The Wi-Fi interface the report asks wpa_supplicant about: the wifi
/// service's own `--iface`, else the board profile's `[wifi] iface`, else
/// the wifi crate's fallback — the same order the wifi service uses (PER-04).
pub fn wifi_iface(wifi_args: &[String], board: Option<&board::Wifi>) -> String {
    wifi_args
        .windows(2)
        .rev()
        .find(|pair| pair[0] == "--iface")
        .map(|pair| pair[1].clone())
        .or_else(|| board.map(|wifi| wifi.iface.clone()))
        .unwrap_or_else(|| wifi::hardware::FALLBACK_IFACE.to_owned())
}

/// Waits for the camera to be ready, then prints the report once.
pub async fn watch(
    root: ActorRef<Supervisor>,
    console: Option<Console>,
    start: StartTime,
    wpa_dirs: Vec<PathBuf>,
    iface: String,
) {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut ticker = tokio::time::interval(POLL);
    let (statuses, ready) = loop {
        ticker.tick().await;
        let Ok(statuses) = root.ask(StatusAll).await else {
            return; // supervisor is shutting down
        };
        let services_up = waiting_for(&statuses).is_empty();
        let addressed = tokio::task::spawn_blocking(|| !ipv4_interfaces().is_empty())
            .await
            .unwrap_or(false);
        if services_up && addressed {
            break (statuses, true);
        }
        if tokio::time::Instant::now() >= deadline {
            break (statuses, false);
        }
    };

    let report = tokio::task::spawn_blocking(move || {
        let net = Network::probe(&wpa_dirs, &iface);
        let ports = service_ports(&statuses);
        render(&start, &Stamp::now(), ready, &statuses, &net, &ports)
    })
    .await;
    let Ok(report) = report else { return };

    tracing::info!(ready, "ready report");
    if let Some(console) = console {
        console.print(report);
    } else {
        print!("{report}");
    }
}

/// Enabled services that are not yet settled.
fn waiting_for(statuses: &[ServiceStatus]) -> Vec<&str> {
    statuses
        .iter()
        .filter(|s| s.enabled)
        .filter(|s| s.state != State::Running || s.uptime_secs.unwrap_or(0) < SETTLE.as_secs())
        .map(|s| s.name.as_str())
        .collect()
}

/// What the report says about the network.
#[derive(Debug, Default)]
struct Network {
    ssid: Option<String>,
    /// Signal in dBm.
    signal_dbm: Option<i32>,
    /// `(interface, address, gateway)`.
    ipv4: Vec<(String, String, Option<String>)>,
    /// `None` when the kernel has no IPv6; else `(interface, address, scope)`.
    ipv6: Option<Vec<(String, Ipv6Addr, &'static str)>>,
}

impl Network {
    fn probe(wpa_dirs: &[PathBuf], iface: &str) -> Self {
        let status = wpa_dirs
            .iter()
            .find_map(|dir| wifi::wpa::query(dir, iface, "STATUS").ok());
        let ssid = status.and_then(|reply| wifi::wpa::Status::parse(&reply).ssid);
        let signal_dbm = wpa_dirs
            .iter()
            .find_map(|dir| wifi::wpa::query(dir, iface, "SIGNAL_POLL").ok())
            .and_then(|reply| parse_signal_poll(&reply))
            .or_else(|| {
                std::fs::read_to_string("/proc/net/wireless")
                    .ok()
                    .and_then(|text| parse_proc_wireless(&text, iface))
            });
        let ipv4 = ipv4_interfaces()
            .into_iter()
            .map(|(iface, ip)| {
                let gateway = wifi::net::default_gateway(&iface).map(|gw| gw.to_string());
                (iface, ip, gateway)
            })
            .collect();
        let ipv6 = std::fs::read_to_string("/proc/net/if_inet6")
            .ok()
            .map(|text| parse_if_inet6(&text));
        Self {
            ssid,
            signal_dbm,
            ipv4,
            ipv6,
        }
    }
}

/// Non-loopback interfaces with an IPv4 address.
fn ipv4_interfaces() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|iface| iface != "lo")
        .filter_map(|iface| {
            let ip = wifi::net::ipv4_address(&iface)?;
            Some((iface, ip.to_string()))
        })
        .collect();
    found.sort();
    found
}

/// `RSSI=-55` from wpa_supplicant's `SIGNAL_POLL`.
fn parse_signal_poll(reply: &str) -> Option<i32> {
    reply
        .lines()
        .find_map(|line| line.strip_prefix("RSSI="))
        .and_then(|v| v.trim().parse().ok())
        .filter(|dbm: &i32| *dbm < 0)
}

/// Signal level of `iface` from `/proc/net/wireless`. Some drivers report
/// dBm as an unsigned byte (`201.` for -55), which is undone here.
fn parse_proc_wireless(text: &str, iface: &str) -> Option<i32> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("{iface}:")))?;
    let level = line.split_whitespace().nth(3)?.trim_end_matches('.');
    let level: i32 = level.parse().ok()?;
    let dbm = if level > 63 { level - 256 } else { level };
    (dbm < 0).then_some(dbm)
}

/// `/proc/net/if_inet6`: `addr ifindex prefix scope flags name`.
fn parse_if_inet6(text: &str) -> Vec<(String, Ipv6Addr, &'static str)> {
    let mut out: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || f[5] == "lo" {
                return None;
            }
            let raw = u128::from_str_radix(f[0], 16).ok()?;
            let scope = match f[3] {
                "00" => "global",
                "20" => "link",
                "40" => "site",
                _ => "other",
            };
            Some((f[5].to_owned(), Ipv6Addr::from(raw), scope))
        })
        .collect();
    out.sort();
    out
}

/// One listening socket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Port {
    proto: &'static str,
    port: u16,
}

/// Listening sockets in a `/proc/net/{tcp,udp}{,6}` table, by inode.
/// TCP counts in state `0A` (LISTEN); UDP counts when bound to a port and
/// not connected.
fn parse_proc_net(text: &str, proto: &'static str) -> HashMap<u64, Port> {
    let mut out = HashMap::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let local_port = f[1]
            .rsplit(':')
            .next()
            .and_then(|p| u16::from_str_radix(p, 16).ok());
        let remote_port = f[2]
            .rsplit(':')
            .next()
            .and_then(|p| u16::from_str_radix(p, 16).ok());
        let listening = if proto.starts_with("tcp") {
            f[3] == "0A"
        } else {
            remote_port == Some(0)
        };
        let (Some(port), true, Ok(inode)) = (local_port, listening, f[9].parse::<u64>()) else {
            continue;
        };
        if port != 0 && inode != 0 {
            let proto = if proto.starts_with("tcp") {
                "tcp"
            } else {
                "udp"
            };
            out.insert(inode, Port { proto, port });
        }
    }
    out
}

/// Ports each running service (or a direct child of it) listens on.
fn service_ports(statuses: &[ServiceStatus]) -> HashMap<String, BTreeSet<Port>> {
    let mut sockets = HashMap::new();
    for table in ["tcp", "tcp6", "udp", "udp6"] {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/net/{table}")) {
            sockets.extend(parse_proc_net(&text, table));
        }
    }
    let children = child_pids();
    statuses
        .iter()
        .filter_map(|s| Some((s.name.clone(), s.pid?)))
        .map(|(name, pid)| {
            let mut pids = vec![pid];
            pids.extend(children.get(&pid).into_iter().flatten().copied());
            let ports = pids
                .iter()
                .flat_map(|pid| socket_inodes(*pid))
                .filter_map(|inode| sockets.get(&inode).cloned())
                .collect();
            (name, ports)
        })
        .collect()
}

/// `parent pid -> child pids`, from `/proc/<pid>/stat`.
fn child_pids() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command name is in parentheses and may contain spaces.
        if let Some(ppid) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
        {
            map.entry(ppid).or_default().push(pid);
        }
    }
    map
}

/// Socket inodes held open by `pid`.
fn socket_inodes(pid: u32) -> Vec<u64> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    fds.flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|target| {
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

fn render(
    start: &StartTime,
    now: &Stamp,
    ready: bool,
    statuses: &[ServiceStatus],
    net: &Network,
    ports: &HashMap<String, BTreeSet<Port>>,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push_str(RESET);
        out.push_str("\r\n");
    };
    let field = |name: &str, value: &str| format!("    {CYAN}{BOLD}{name:<10}{RESET} {value}");
    let now_uptime = now.uptime;
    let stamp = now.text();
    let took = now_uptime - start.uptime;

    line(String::new());
    if ready {
        line(format!(
            "{DIM}{stamp}{RESET} {BLACK_ON_GREEN} READY {RESET} {BOLD}{} ready in {took:.1} s{RESET} {DIM}({now_uptime:.1} s after kernel boot)",
            crate::banner::PROJECT
        ));
    } else {
        line(format!(
            "{DIM}{stamp}{RESET} {WHITE_ON_RED} NOT READY {RESET} {BOLD}after {took:.0} s{RESET}, still waiting for: {}",
            missing(statuses, net)
        ));
    }

    let wifi = match (&net.ssid, net.signal_dbm) {
        (Some(ssid), Some(dbm)) => format!("{BOLD}{ssid}{RESET}  {}", signal(dbm)),
        (Some(ssid), None) => format!("{BOLD}{ssid}{RESET}  {DIM}(signal unknown)"),
        (None, Some(dbm)) => format!("{DIM}(SSID unknown){RESET}  {}", signal(dbm)),
        (None, None) => format!("{DIM}not associated"),
    };
    line(field("Wi-Fi", &wifi));

    if net.ipv4.is_empty() {
        line(field("IPv4", &format!("{DIM}no address")));
    }
    for (iface, ip, gateway) in &net.ipv4 {
        let gw = gateway
            .as_ref()
            .map(|g| format!("  {DIM}gateway {g}"))
            .unwrap_or_default();
        line(field(
            "IPv4",
            &format!("{BOLD}{ip}{RESET}  {DIM}{iface}{RESET}{gw}"),
        ));
    }
    match &net.ipv6 {
        None => line(field("IPv6", &format!("{DIM}not supported by this kernel"))),
        Some(addrs) if addrs.is_empty() => line(field("IPv6", &format!("{DIM}no address"))),
        Some(addrs) => {
            for (iface, addr, scope) in addrs {
                line(field("IPv6", &format!("{addr}  {DIM}{iface}, {scope}")));
            }
        }
    }

    let mut first = true;
    for status in statuses.iter().filter(|s| s.enabled) {
        let listening = ports.get(&status.name).filter(|p| !p.is_empty()).map(|p| {
            p.iter()
                .map(|port| format!("{}/{}", port.proto, port.port))
                .collect::<Vec<_>>()
                .join(" ")
        });
        let value = match (status.state == State::Running, listening) {
            (true, Some(ports)) => format!("{:<11} {YELLOW}{ports}", status.name),
            (true, None) => format!("{:<11} {DIM}no listening ports", status.name),
            (false, _) => format!("{:<11} {DIM}{:?}", status.name, status.state),
        };
        line(field(if first { "Services" } else { "" }, &value));
        first = false;
    }

    // The routed interface is the one to reach the camera on.
    let reachable = net
        .ipv4
        .iter()
        .find(|(_, _, gw)| gw.is_some())
        .or(net.ipv4.first());
    if let Some((_, ip, _)) = reachable {
        let mut hints = Vec::new();
        for name in ["web", "sshd", "onvif"] {
            let Some(set) = ports.get(name) else { continue };
            let tcp = set.iter().find(|p| p.proto == "tcp");
            match (name, tcp) {
                ("web", Some(p)) => hints.push(format!("http://{ip}:{}/", p.port)),
                ("sshd", Some(p)) => hints.push(format!("ssh -p {} root@{ip}", p.port)),
                ("onvif", Some(p)) => {
                    hints.push(format!("onvif http://{ip}:{}/onvif/device_service", p.port))
                }
                _ => {}
            }
        }
        if !hints.is_empty() {
            line(field("Connect", &format!("{BOLD}{}", hints.join("   "))));
        }
    }
    line(String::new());
    out
}

/// What the NOT READY report is still waiting for.
fn missing(statuses: &[ServiceStatus], net: &Network) -> String {
    let mut items: Vec<String> = waiting_for(statuses)
        .iter()
        .map(|s| s.to_string())
        .collect();
    if net.ipv4.is_empty() {
        items.push("an IPv4 address".into());
    }
    if items.is_empty() {
        "nothing (settled just now)".into()
    } else {
        items.join(", ")
    }
}

/// `-55 dBm (good)`.
fn signal(dbm: i32) -> String {
    let (word, colour) = match dbm {
        d if d >= -60 => ("good", "\x1b[32m"),
        d if d >= -70 => ("fair", YELLOW),
        _ => ("weak", "\x1b[31m"),
    };
    format!("{colour}{dbm} dBm{RESET} {DIM}({word})")
}

/// `YYYY-MM-DD HH:MM:SS -0300`: Unix time `epoch` in the local zone.
pub fn local(epoch: u64) -> String {
    os::time::local(epoch as i64, 0).seconds()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_listeners_and_bound_udp_are_found() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1111 1 c0000000 100 0 0 10 0\n\
   1: 00000000:08AE 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2222 1 c0000000 100 0 0 10 0\n\
   2: BB14A8C0:08AE 0114A8C0:D431 01 00000000:00000000 00:00000000 00000000     0        0 3333 1 c0000000 100 0 0 10 0\n";
        let found = parse_proc_net(tcp, "tcp");
        assert_eq!(
            found.get(&1111),
            Some(&Port {
                proto: "tcp",
                port: 80
            })
        );
        assert_eq!(
            found.get(&2222),
            Some(&Port {
                proto: "tcp",
                port: 2222
            })
        );
        assert!(
            !found.contains_key(&3333),
            "established connection is not a listener"
        );

        let udp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
  10: 00000000:0E76 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 4444 2 c0000000 0\n";
        assert_eq!(
            parse_proc_net(udp, "udp").get(&4444),
            Some(&Port {
                proto: "udp",
                port: 3702
            })
        );
    }

    #[test]
    fn signal_from_wpa_and_proc() {
        assert_eq!(
            parse_signal_poll("RSSI=-55\nLINKSPEED=65\nNOISE=9999\nFREQUENCY=2437\n"),
            Some(-55)
        );
        assert_eq!(parse_signal_poll("FAIL\n"), None);
        let proc = "Inter-| sta-|   Quality        |   Discarded packets\n face | tus | link level noise |  nwid  crypt\n wlan0: 0000   70.  -40.  -256        0      0\n";
        assert_eq!(parse_proc_wireless(proc, "wlan0"), Some(-40));
        let unsigned = " wlan0: 0000   70.  201.  0        0      0\n";
        assert_eq!(parse_proc_wireless(unsigned, "wlan0"), Some(-55));
    }

    #[test]
    fn ipv6_addresses_with_scope() {
        let text = "fe800000000000000211223344556677 03 40 20 80    wlan0\n\
                    00000000000000000000000000000001 01 80 10 80       lo\n";
        let addrs = parse_if_inet6(text);
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].0, "wlan0");
        assert_eq!(addrs[0].1.to_string(), "fe80::211:2233:4455:6677");
        assert_eq!(addrs[0].2, "link");
    }

    #[test]
    fn wpa_ctrl_dir_from_wifi_args_comes_first() {
        let args = vec![
            "--config".into(),
            "camera.conf".into(),
            "--ctrl-dir".into(),
            "/x".into(),
        ];
        assert_eq!(wpa_ctrl_dirs(&args)[0], PathBuf::from("/x"));
    }

    #[test]
    fn wifi_iface_precedence() {
        let wifi = board::Wifi {
            iface: "wlan2".into(),
            module: None,
            module_args: Vec::new(),
            power_gpio: None,
        };
        // No profile, no flag: the fallback.
        assert_eq!(wifi_iface(&[], None), "wlan0");
        // The profile.
        assert_eq!(
            wifi_iface(&["--config".into(), "c".into()], Some(&wifi)),
            "wlan2"
        );
        // The wifi service's own flag wins.
        let args = vec!["--iface".into(), "wlan5".into()];
        assert_eq!(wifi_iface(&args, Some(&wifi)), "wlan5");
        assert_eq!(wifi_iface(&args, None), "wlan5");
    }

    #[test]
    fn wifi_iface_from_the_shipped_av100_profile() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../boards/anyka-av100-ptz.toml");
        let profile = board::load(&path).unwrap();
        assert_eq!(wifi_iface(&[], profile.wifi.as_ref()), "wlan0");
    }

    #[test]
    fn banner_rows_show_local_time_once_the_clock_is_set() {
        let unset = StartTime {
            uptime: 4.5,
            wall: None,
        };
        let [(_, boot), (_, start)] = unset.banner_rows();
        assert!(boot.contains("clock not set yet"), "{boot}");
        assert_eq!(start, "uptime 4.50 s");

        let set = StartTime {
            uptime: 4.5,
            wall: Some(1_791_997_387),
        };
        let [(_, boot), (_, start)] = set.banner_rows();
        // The offset depends on the host's TZ; the shape does not.
        let shape = |s: &str| {
            let stamp = &s[..25];
            assert!(stamp.starts_with("2026-10-1"), "{s}");
            assert!(matches!(&stamp[20..21], "+" | "-"), "{s}");
        };
        shape(&boot);
        shape(&start);
        assert!(start.contains("(uptime 4.50 s)"), "{start}");
    }
}
