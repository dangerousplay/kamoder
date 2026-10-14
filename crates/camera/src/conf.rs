//! The AV100 ISP tuning file (`/usr/local/isp_<sensor>_mipi_2lane_mipiN.conf`,
//! format version 7) and the ISP commands it turns into.
//!
//! A file is a run of *subfiles*, one per parameter set (slot 0 day, 1 night,
//! 2–4 user). Each subfile is a 0x200-byte header followed by modules
//! `{u16 id, u16 len, data}`: ids 0..=23 are ISP blocks whose `len` counts the
//! 4-byte header, then id 28, the sensor's register table, whose `len` does
//! not. The vendor library sends each ISP block (or several structs inside
//! it) to `/dev/isp-param-N` by pointer; [`IspCommand`] lists exactly what it
//! sends, in order, recovered from `libplat_vpss` and checked byte for byte
//! against traces of the vendor stack.

use snafu::{ensure, OptionExt, Snafu};

/// Format version this module understands.
pub const VERSION: u32 = 7;
const HEADER: usize = 0x200;
const SENSOR_TABLE_ID: u16 = 28;

/// Length (header included) of ISP modules 0..=23, as the vendor library's
/// table requires them.
const MODULE_LEN: [u16; 24] = [
    0x13c, 0xd4, 0x378, 0x12ac, 0x1ba8, 0x90, 0x16, 0x6d6, 0x148, 0xb4, 0x422c, 0xb610, 0x15c,
    0x6c, 0x8, 0xe, 0xd4, 0x5e0, 0x2e, 0x1c28, 0x73c, 0x2a, 0x132, 0x8a0,
];

/// Module id → `(ISP ioctl number, offset in the module incl. header, size)`.
/// Most modules are one struct at offset 4; a few hold several.
const COMMANDS: [&[(u32, usize, usize)]; 24] = [
    &[(0x4004_4902, 4, 0x138)], // 0 BLC
    &[(0x4004_4904, 4, 0xd0)],  // 1 LSC
    &[(0x4004_490a, 4, 0x374)], // 2 RAW_LUT
    &[
        (0x4004_490c, 4, 0xbf8),
        (0x4004_492b, 0xbfc, 0x3e0),
        (0x4004_497d, 0xfdc, 0x2d0),
    ], // 3 NR
    &[(0x4004_492f, 4, 0x1ba4)], // 4 3D-NR
    &[(0x4004_4906, 4, 0x8c)],  // 5 GB
    &[(0x4004_490e, 4, 0x12)],  // 6 DEMO
    &[(0x4004_491a, 4, 0x6d2)], // 7 RGB gamma
    &[(0x4004_4912, 4, 0x144)], // 8 CCM
    &[(0x4004_4936, 4, 0xb0)],  // 9 FCS
    &[(0x4004_491b, 4, 0x4228)], // 10 WDR
    &[(0x4004_4925, 4, 0xb5f8), (0x4004_4927, 0xb5fc, 0x14)], // 11 SHARP
    &[(0x4004_493b, 4, 0x158)], // 12 SAT
    &[(0x4004_4939, 4, 0x68)],  // 13 CONTRAST
    &[(0x4004_493f, 4, 4)],     // 14 RGB2YUV
    &[(0x4004_4941, 4, 0xa)],   // 15 EFFECT
    &[(0x4004_4910, 4, 0xd0)],  // 16 DPC
    &[(0x8004_498e, 4, 0x5dc)], // 17 LCE
    &[(0x4004_4960, 4, 0x2a)],  // 18 AF
    &[
        (0x8004_4986, 4, 0x7e0),
        (0x4004_4973, 0x7e4, 0x1404),
        (0x8004_4988, 0x1be8, 0x40),
    ], // 19 WB
    &[(0x8004_498a, 4, 0x720), (0x4004_494e, 0x724, 0x18)], // 20 AE
    &[(0x4004_496e, 4, 0x26)],  // 21 MISC
    &[(0x4004_4975, 4, 0x12e)], // 22 Y gamma
    &[(0x4004_4977, 4, 0x89c)], // 23 HUE
];

/// `AK_ISP_SET_SENSOR_FPS`: the sensor frame rate, an `int`.
pub const SET_SENSOR_FPS: u32 = 0x4004_497a;
/// `AK_ISP_SET_AE_ATTR` (the 0x720-byte exposure struct of module 20).
pub const SET_EXPOSURE: u32 = 0x8004_498a;
/// `AK_ISP_SET_3D_NR_REF`: `{u32 width, u32 height}` of the 3D-NR reference.
pub const SET_3D_NR_REF: u32 = 0x4004_4967;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum Error {
    #[snafu(display("ISP config is truncated at byte {offset}"))]
    Truncated { offset: usize },
    #[snafu(display("ISP config version {version}, expected {VERSION}"))]
    Version { version: u32 },
    #[snafu(display("ISP config module at byte {offset} is id {id} len {len}, expected id {expected} len {expected_len}"))]
    Module {
        offset: usize,
        id: u16,
        len: u16,
        expected: u16,
        expected_len: u16,
    },
    #[snafu(display("ISP config has no parameter set for slot {slot}"))]
    NoSlot { slot: u8 },
}

/// One parameter set.
#[derive(Debug, Clone)]
pub struct Subfile {
    /// 0 day, 1 night, 2–4 user.
    pub slot: u8,
    pub sensor_id: u32,
    /// Modules 0..=23, header included.
    pub modules: Vec<Vec<u8>>,
    /// Sensor register writes `(reg, value)`, as the subdev init takes them.
    pub sensor_regs: Vec<(u16, u16)>,
}

/// One command to `/dev/isp-param-N`, in sending order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IspCommand {
    pub cmd: u32,
    pub data: Vec<u8>,
}

fn u16_at(d: &[u8], at: usize) -> Result<u16, Error> {
    let b = d.get(at..at + 2).context(TruncatedSnafu { offset: at })?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], at: usize) -> Result<u32, Error> {
    let b = d.get(at..at + 4).context(TruncatedSnafu { offset: at })?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Every subfile in the file, in order.
pub fn parse(data: &[u8]) -> Result<Vec<Subfile>, Error> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + HEADER <= data.len() {
        let version = u32_at(data, at)?;
        ensure!(version == VERSION, VersionSnafu { version });
        let sensor_id = u32_at(data, at + 0x14)?;
        let slot = data[at + 0x1f];
        let mut m = at + HEADER;
        let mut modules = Vec::with_capacity(24);
        for (expected, &expected_len) in MODULE_LEN.iter().enumerate() {
            let (id, len) = (u16_at(data, m)?, u16_at(data, m + 2)?);
            ensure!(
                usize::from(id) == expected && len == expected_len,
                ModuleSnafu {
                    offset: m,
                    id,
                    len,
                    expected: expected as u16,
                    expected_len
                }
            );
            let body = data
                .get(m..m + usize::from(len))
                .context(TruncatedSnafu { offset: m })?;
            modules.push(body.to_vec());
            m += usize::from(len);
        }
        let (id, len) = (u16_at(data, m)?, u16_at(data, m + 2)?);
        ensure!(
            id == SENSOR_TABLE_ID,
            ModuleSnafu {
                offset: m,
                id,
                len,
                expected: SENSOR_TABLE_ID,
                expected_len: len
            }
        );
        let table = data
            .get(m + 4..m + 4 + usize::from(len))
            .context(TruncatedSnafu { offset: m })?;
        let sensor_regs = table
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                (
                    u16::from_le_bytes([p[0], p[1]]),
                    u16::from_le_bytes([p[2], p[3]]),
                )
            })
            .collect();
        out.push(Subfile {
            slot,
            sensor_id,
            modules,
            sensor_regs,
        });
        at = m + 4 + usize::from(len);
    }
    if out.is_empty() {
        return TruncatedSnafu { offset: data.len() }.fail();
    }
    Ok(out)
}

/// The subfile for `slot`.
pub fn select(subfiles: &[Subfile], slot: u8) -> Result<&Subfile, Error> {
    subfiles
        .iter()
        .find(|s| s.slot == slot)
        .context(NoSlotSnafu { slot })
}

/// Sensor frame-rate block inside module 20: `(fps, max_exposure, gain)`
/// for high light then low light.
fn frame_rate(sub: &Subfile) -> [u32; 6] {
    let m = &sub.modules[20];
    std::array::from_fn(|i| u32_at(m, 0x724 + 4 * i).unwrap_or(0))
}

/// Exposure step for 50 Hz anti-flicker, as the vendor computes it.
fn anti_flicker_step(exp_max: u32, fps: u32) -> u32 {
    match fps {
        12 | 13 => exp_max * 125 / 1000,
        14 => exp_max * 1428 / 10000,
        _ => exp_max * fps / 100,
    }
}

impl Subfile {
    /// Frame rate the sensor runs at in good light.
    pub fn fps(&self) -> u32 {
        frame_rate(self)[0]
    }

    /// The ISP commands of a full load (day start-up), in vendor order:
    /// modules 0..=23 with the sensor frame rate set right after module 20,
    /// then the exposure attributes with the anti-flicker step filled in.
    /// The sensor register table goes to the subdev in between (see
    /// [`Subfile::sensor_regs`]); [`Self::exposure`] comes after it.
    pub fn commands(&self) -> Vec<IspCommand> {
        let mut out = Vec::new();
        for (id, module) in self.modules.iter().enumerate() {
            for &(cmd, off, size) in COMMANDS[id] {
                out.push(IspCommand {
                    cmd,
                    data: module[off..off + size].to_vec(),
                });
            }
            if id == 20 {
                out.push(IspCommand {
                    cmd: SET_SENSOR_FPS,
                    data: (self.fps() as i32).to_le_bytes().to_vec(),
                });
            }
        }
        out
    }

    /// The exposure attributes sent after the sensor table.
    pub fn exposure(&self) -> IspCommand {
        let mut e = self.modules[20][4..4 + 0x720].to_vec();
        if u32_at(&e, 0x3c).unwrap_or(1) != 1 {
            let step = anti_flicker_step(u32_at(&e, 0x1c).unwrap_or(0), self.fps());
            e[0x3c..0x40].copy_from_slice(&step.to_le_bytes());
        }
        IspCommand {
            cmd: SET_EXPOSURE,
            data: e,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic one- or two-slot file with recognisable module bytes.
    pub(crate) fn sample(slots: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        for &slot in slots {
            let mut h = vec![0u8; HEADER];
            h[0..4].copy_from_slice(&VERSION.to_le_bytes());
            h[0x14..0x18].copy_from_slice(&0x2331u32.to_le_bytes());
            h[0x1f] = slot;
            d.extend_from_slice(&h);
            for (id, &len) in MODULE_LEN.iter().enumerate() {
                d.extend_from_slice(&(id as u16).to_le_bytes());
                d.extend_from_slice(&len.to_le_bytes());
                d.extend((4..len).map(|i| (i as u8) ^ (id as u8) ^ slot));
                if id == 20 {
                    // fps 15, max exposure 4512; anti-flicker step field set.
                    let base = d.len() - usize::from(len);
                    d[base + 0x724..base + 0x728].copy_from_slice(&15u32.to_le_bytes());
                    d[base + 4 + 0x1c..base + 4 + 0x20].copy_from_slice(&4512u32.to_le_bytes());
                    d[base + 4 + 0x3c..base + 4 + 0x40].copy_from_slice(&0u32.to_le_bytes());
                }
            }
            let regs: [(u16, u16); 3] = [(0x0103, 1), (0x0100, 1), (0, 0)];
            d.extend_from_slice(&SENSOR_TABLE_ID.to_le_bytes());
            d.extend_from_slice(&((regs.len() * 4) as u16).to_le_bytes());
            for (r, v) in regs {
                d.extend_from_slice(&r.to_le_bytes());
                d.extend_from_slice(&v.to_le_bytes());
            }
        }
        d
    }

    #[test]
    fn parses_day_and_night() {
        let subs = parse(&sample(&[0, 1])).unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(select(&subs, 1).unwrap().slot, 1);
        let day = select(&subs, 0).unwrap();
        assert_eq!(day.sensor_id, 0x2331);
        assert_eq!(day.sensor_regs, vec![(0x0103, 1), (0x0100, 1), (0, 0)]);
        assert_eq!(day.fps(), 15);
        assert_eq!(select(&subs, 2).unwrap_err(), Error::NoSlot { slot: 2 });
    }

    #[test]
    fn commands_follow_vendor_order() {
        let subs = parse(&sample(&[0])).unwrap();
        let cmds = subs[0].commands();
        // 24 modules → 30 structs, plus the sensor frame rate.
        assert_eq!(cmds.len(), 31);
        assert_eq!(cmds[0].cmd, 0x4004_4902);
        assert_eq!(cmds[0].data, subs[0].modules[0][4..4 + 0x138]);
        let fps = cmds.iter().position(|c| c.cmd == SET_SENSOR_FPS).unwrap();
        assert_eq!(cmds[fps - 1].cmd, 0x4004_494e);
        assert_eq!(cmds[fps].data, 15i32.to_le_bytes());
        assert_eq!(cmds.last().unwrap().cmd, 0x4004_4977);
        let e = subs[0].exposure();
        assert_eq!(&e.data[0x3c..0x40], &(4512u32 * 15 / 100).to_le_bytes());
    }

    #[test]
    fn rejects_bad_files() {
        let mut d = sample(&[0]);
        d[0] = 3;
        assert_eq!(parse(&d).unwrap_err(), Error::Version { version: 3 });
        let d = sample(&[0]);
        assert!(matches!(
            parse(&d[..HEADER + 100]),
            Err(Error::Truncated { .. })
        ));
    }
}

/// Byte-for-byte check against a real tuning file and the structs the vendor
/// stack was traced sending (`KAMODER_ISP_CONF` + `KAMODER_ISP_DUMPS`, files
/// named `isp-param-0-<cmd>-<seq>-in.bin`). Skipped when unset: the vendor
/// files are not redistributed.
#[cfg(test)]
mod vendor {
    use super::*;

    #[test]
    fn matches_traced_vendor_payloads() {
        let (Ok(conf), Ok(dumps)) = (
            std::env::var("KAMODER_ISP_CONF"),
            std::env::var("KAMODER_ISP_DUMPS"),
        ) else {
            return;
        };
        let subs = parse(&std::fs::read(conf).unwrap()).unwrap();
        let day = select(&subs, 0).unwrap();
        let mut sent: Vec<IspCommand> = day.commands();
        sent.push(day.exposure());
        let mut checked = 0;
        for entry in std::fs::read_dir(dumps).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let cmd = u32::from_str_radix(name.split('-').nth(3).unwrap(), 16).unwrap();
            let dump = std::fs::read(&path).unwrap();
            let candidates: Vec<_> = sent.iter().filter(|c| c.cmd == cmd).collect();
            if candidates.is_empty() {
                continue; // GETs and the 3D-NR size are not conf data
            }
            assert!(
                candidates.iter().any(|c| {
                    let n = c.data.len().min(dump.len());
                    c.data[..n] == dump[..n]
                }),
                "{name} differs"
            );
            checked += 1;
        }
        assert!(checked >= 30, "only {checked} dumps checked");
    }
}
