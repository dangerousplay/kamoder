//! Raw system counters for the dashboard's live graphs.
//!
//! `/api/system/stats` returns cumulative counters (CPU jiffies, interface
//! bytes) plus instantaneous memory; the browser polls once a second and
//! turns consecutive samples into rates, so the server keeps no history and
//! any number of viewers cost the same.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::path::Path;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/system/stats", get(stats))
}

/// `api.ts` `SystemStats`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Stats {
    /// Seconds since boot (`/proc/uptime`), the time base for rates.
    pub uptime: f64,
    /// Aggregate CPU jiffies: busy (everything but idle and iowait) and total.
    pub cpu_busy: u64,
    pub cpu_total: u64,
    pub load1: f32,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub mem_free_kb: u64,
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

fn kib(meminfo: &str, key: &str) -> u64 {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// `cpu  user nice system idle iowait irq softirq steal …`.
fn cpu(stat: &str) -> (u64, u64) {
    let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    let total: u64 = v.iter().take(8).sum();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    (total.saturating_sub(idle), total)
}

/// `/proc/net/dev`, without the loopback.
fn interfaces(dev: &str) -> Vec<Interface> {
    dev.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let name = name.trim();
            let f: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            (name != "lo" && f.len() >= 9).then(|| Interface {
                name: name.to_owned(),
                rx_bytes: f[0],
                tx_bytes: f[8],
            })
        })
        .collect()
}

pub fn read(root: &Path) -> Stats {
    let text = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap_or_default();
    let meminfo = text("proc/meminfo");
    let (cpu_busy, cpu_total) = cpu(&text("proc/stat"));
    Stats {
        uptime: text("proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|u| u.parse().ok())
            .unwrap_or(0.0),
        cpu_busy,
        cpu_total,
        load1: text("proc/loadavg")
            .split_whitespace()
            .next()
            .and_then(|l| l.parse().ok())
            .unwrap_or(0.0),
        mem_total_kb: kib(&meminfo, "MemTotal"),
        mem_available_kb: kib(&meminfo, "MemAvailable"),
        mem_free_kb: kib(&meminfo, "MemFree"),
        interfaces: interfaces(&text("proc/net/dev")),
    }
}

/// `GET /api/system/stats`.
async fn stats(State(state): State<AppState>, _: AuthUser) -> Result<Json<Stats>, ApiError> {
    let root = state.config.sys_root.clone();
    let s = tokio::task::spawn_blocking(move || read(&root))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc() {
        let root = crate::tests::scratch("stats");
        let w = |rel: &str, t: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w(
            "proc/stat",
            "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 100 0 50 800 50 0 0 0 0 0\n",
        );
        w(
            "proc/meminfo",
            "MemTotal:  32788 kB\nMemFree:  700 kB\nMemAvailable:  13000 kB\n",
        );
        w("proc/uptime", "1303.67 246.99\n");
        w("proc/loadavg", "0.52 0.40 0.30 1/62 984\n");
        w(
            "proc/net/dev",
            "Inter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes\n    lo: 10 1 0 0 0 0 0 0 10 1 0 0 0 0 0 0\n wlan0: 5000 40 0 0 0 0 0 0 7000 50 0 0 0 0 0 0\n",
        );
        let s = read(&root);
        assert_eq!((s.cpu_busy, s.cpu_total), (150, 1000));
        assert_eq!(s.mem_available_kb, 13000);
        assert_eq!(s.uptime, 1303.67);
        assert_eq!(s.load1, 0.52);
        assert_eq!(
            s.interfaces,
            vec![Interface {
                name: "wlan0".into(),
                rx_bytes: 5000,
                tx_bytes: 7000
            }]
        );
    }
}
