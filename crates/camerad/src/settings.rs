//! What the service runs with: the board profile's `[video]` section, with
//! command-line flags on top (CAM-04, BRD-05).
//!
//! Precedence, per value: a flag given on the command line, then the
//! sensor's `[[video.override]]`, then `[video.stream]` / `[video.snapshot]`.
//! Kept free of I/O so the merge is tested on the host.

use board::{Stream, Video};

/// `WxH`, or `off` (`None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size(pub Option<(u32, u32)>);

pub fn parse_size(s: &str) -> Result<Size, String> {
    if s == "off" {
        return Ok(Size(None));
    }
    let (w, h) = s.split_once('x').ok_or("expected WxH or off")?;
    let w = w.parse::<u32>().map_err(|e| e.to_string())?;
    let h = h.parse::<u32>().map_err(|e| e.to_string())?;
    if w < 32 || h < 18 || w % 2 != 0 || h % 2 != 0 {
        return Err("size too small or odd".into());
    }
    Ok(Size(Some((w, h))))
}

/// Stream flags as given; `None` = not on the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamFlags {
    pub size: Option<Size>,
    pub fps: Option<u32>,
    pub kbps: Option<u32>,
    pub gop: Option<u32>,
}

/// Stream of sensor `index`: the profile's (with its override), then the
/// flags. `None` when the stream is switched off (`--stream off`).
pub fn stream_for(video: &Video, index: u32, flags: &StreamFlags) -> Option<Stream> {
    let mut s = video.stream_for(index);
    match flags.size {
        Some(Size(None)) => return None,
        Some(Size(Some((w, h)))) => (s.width, s.height) = (w, h),
        None => {}
    }
    s.fps = flags.fps.unwrap_or(s.fps);
    s.kbps = flags.kbps.unwrap_or(s.kbps);
    s.gop = flags.gop.unwrap_or(s.gop);
    Some(s)
}

/// Snapshot settings: flags over `[video.snapshot]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Pictures are halved while wider than this (`u32::MAX`: never).
    pub max_width: u32,
    pub quality: u8,
}

pub fn snapshot(video: &Video, max_width: Option<u32>, quality: Option<u8>) -> Snapshot {
    Snapshot {
        max_width: max_width.or(video.snapshot.max_width).unwrap_or(u32::MAX),
        quality: quality.unwrap_or(video.snapshot.quality),
    }
}

/// Sensors to bring up: the ones asked for (flags, else the profile) that
/// the kernel has, and the asked-for ones it lacks (to warn about).
pub fn select_sensors(profile: &[u32], detected: &[u32], flags: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let wanted = if flags.is_empty() { profile } else { flags };
    let mut serve = Vec::new();
    let mut missing = Vec::new();
    for &i in wanted {
        if serve.contains(&i) || missing.contains(&i) {
            continue;
        }
        if detected.contains(&i) {
            serve.push(i);
        } else {
            missing.push(i);
        }
    }
    (serve, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::StreamOverride;

    fn video() -> Video {
        Video {
            backend: "anyka-av100".into(),
            sensors: vec![0, 1],
            tuning_dir: Some("/usr/local".into()),
            stream: Stream {
                width: 1280,
                height: 720,
                fps: 15,
                kbps: 2048,
                gop: 30,
                profile: "main".into(),
            },
            overrides: vec![StreamOverride {
                index: 1,
                width: Some(640),
                height: Some(360),
                fps: None,
                kbps: Some(1024),
                gop: None,
                profile: Some("baseline".into()),
            }],
            snapshot: board::Snapshot {
                max_width: Some(640),
                quality: 80,
            },
        }
    }

    #[test]
    fn profile_values_without_flags() {
        let v = video();
        let s = stream_for(&v, 0, &StreamFlags::default()).unwrap();
        assert_eq!(s, v.stream);
    }

    #[test]
    fn override_applies_to_its_sensor_only() {
        let v = video();
        let s = stream_for(&v, 1, &StreamFlags::default()).unwrap();
        assert_eq!((s.width, s.height, s.kbps), (640, 360, 1024));
        assert_eq!((s.fps, s.gop, s.profile.as_str()), (15, 30, "baseline"));
    }

    #[test]
    fn flags_beat_profile_and_override() {
        let v = video();
        let flags = StreamFlags {
            size: Some(Size(Some((1920, 1080)))),
            fps: Some(10),
            kbps: Some(4000),
            gop: None,
        };
        for i in [0, 1] {
            let s = stream_for(&v, i, &flags).unwrap();
            assert_eq!((s.width, s.height, s.fps, s.kbps), (1920, 1080, 10, 4000));
        }
        // Not given: still the override's / profile's.
        assert_eq!(stream_for(&v, 1, &flags).unwrap().profile, "baseline");
        assert_eq!(stream_for(&v, 1, &flags).unwrap().gop, 30);
    }

    #[test]
    fn stream_off() {
        let flags = StreamFlags {
            size: Some(Size(None)),
            ..StreamFlags::default()
        };
        assert_eq!(stream_for(&video(), 0, &flags), None);
    }

    #[test]
    fn snapshot_flags_over_profile() {
        let mut v = video();
        assert_eq!(
            snapshot(&v, None, None),
            Snapshot {
                max_width: 640,
                quality: 80
            }
        );
        assert_eq!(snapshot(&v, Some(320), Some(50)).max_width, 320);
        assert_eq!(snapshot(&v, Some(320), Some(50)).quality, 50);
        v.snapshot.max_width = None;
        assert_eq!(snapshot(&v, None, None).max_width, u32::MAX);
    }

    #[test]
    fn sensors_present_and_missing() {
        assert_eq!(select_sensors(&[0, 1], &[0, 1], &[]), (vec![0, 1], vec![]));
        // One sensor unplugged: serve the other alone (no dual mode).
        assert_eq!(select_sensors(&[0, 1], &[1], &[]), (vec![1], vec![0]));
        // Flags replace the profile list.
        assert_eq!(select_sensors(&[0, 1], &[0, 1], &[1]), (vec![1], vec![]));
        assert_eq!(select_sensors(&[0], &[], &[]), (vec![], vec![0]));
        assert_eq!(select_sensors(&[0, 0], &[0], &[]), (vec![0], vec![]));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("off"), Ok(Size(None)));
        assert_eq!(parse_size("640x360"), Ok(Size(Some((640, 360)))));
        assert!(parse_size("641x360").is_err());
        assert!(parse_size("16x16").is_err());
        assert!(parse_size("640").is_err());
    }
}
