//! H.264 Annex-B framing: NAL units, access units, parameter sets.
//!
//! Only byte-stream framing lives here — start codes and the NAL header byte.
//! The start-code scan is `msf_util::h264::extract_nal_unit`, the same routine
//! the RTP packetizer and the SDP fmtp builder use, so all three agree on
//! where a NAL unit ends. Nothing here parses a slice header beyond its first
//! bit.

use bytes::{BufMut, Bytes, BytesMut};

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One encoded picture: Annex-B NAL units with start codes, its presentation
/// time on the 90 kHz RTP clock, and whether a decoder can start from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub data: Bytes,
    pub pts: u64,
    pub keyframe: bool,
}

/// `nal_unit_type` of a NAL unit without its start code.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |header| header & 0x1f)
}

/// The NAL units of an Annex-B byte stream, without start codes. Stops at the
/// first byte that is not part of a start code or a NAL unit.
pub fn nal_units(data: &Bytes) -> impl Iterator<Item = Bytes> {
    let mut rest = data.clone();
    std::iter::from_fn(move || msf_util::h264::extract_nal_unit(&mut rest).ok().flatten())
        .filter(|nal| !nal.is_empty())
}

/// Appends `nal` to `out` with a four-byte start code.
pub fn put_nal(out: &mut BytesMut, nal: &[u8]) {
    out.reserve(START_CODE.len() + nal.len());
    out.put_slice(&START_CODE);
    out.put_slice(nal);
}

/// Groups a NAL unit sequence into access units (H.264 § 7.4.1.2.3).
///
/// A new access unit starts at an access unit delimiter, at SPS/PPS/SEI or
/// the reserved prefix types once the current unit already holds a picture,
/// or at a slice whose `first_mb_in_slice` is 0 (its ue(v) code is the single
/// bit `1`). That is enough for encoder output, which has no arbitrary slice
/// order and no redundant pictures.
#[derive(Default)]
pub struct AccessUnitSplitter {
    current: BytesMut,
    has_picture: bool,
    keyframe: bool,
}

impl AccessUnitSplitter {
    /// Adds one NAL unit; returns the previous access unit (data, keyframe)
    /// when this one starts a new one.
    pub fn push(&mut self, nal: &[u8]) -> Option<(Bytes, bool)> {
        let kind = nal_type(nal);
        let is_slice = (NAL_SLICE..=NAL_IDR).contains(&kind);
        let starts_new = match kind {
            NAL_AUD => true,
            NAL_SEI | NAL_SPS | NAL_PPS | 14..=18 => self.has_picture,
            _ if is_slice => self.has_picture && nal.get(1).is_some_and(|byte| byte & 0x80 != 0),
            _ => false,
        };
        let finished = if starts_new { self.take() } else { None };
        put_nal(&mut self.current, nal);
        if is_slice {
            self.has_picture = true;
            self.keyframe |= kind == NAL_IDR;
        }
        finished
    }

    /// The access unit in progress, if it holds a picture (end of stream).
    pub fn finish(&mut self) -> Option<(Bytes, bool)> {
        self.take()
    }

    fn take(&mut self) -> Option<(Bytes, bool)> {
        let data = std::mem::take(&mut self.current).freeze();
        let unit = (data, self.keyframe);
        let had_picture = std::mem::replace(&mut self.has_picture, false);
        self.keyframe = false;
        had_picture.then_some(unit)
    }
}

/// The SPS and PPS of a stream, without start codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Bytes,
    pub pps: Bytes,
}

impl ParameterSets {
    /// SPS then PPS, as an Annex-B byte stream.
    pub fn annex_b(&self) -> Bytes {
        let mut out = BytesMut::new();
        put_nal(&mut out, &self.sps);
        put_nal(&mut out, &self.pps);
        out.freeze()
    }

    /// The SDP `fmtp` parameters: packetization-mode 1 (FU-A), plus
    /// profile-level-id and sprop-parameter-sets derived from the SPS/PPS.
    pub fn sdp_parameters(&self) -> Option<msf_sdp::h264::H264Parameters> {
        msf_sdp::h264::H264Parameters::new(1, Some(self.annex_b())).ok()
    }
}

/// Tracks the latest SPS/PPS of a stream and puts them in front of every IDR
/// that does not carry its own, so a client joining (or skipping ahead) at
/// any IDR can start decoding even when the encoder only emits them once.
#[derive(Default)]
pub struct ParameterSetCache {
    sps: Option<Bytes>,
    pps: Option<Bytes>,
}

impl ParameterSetCache {
    /// Records the parameter sets in `frame` and, for an IDR missing either
    /// one, returns the frame with the cached ones prepended.
    pub fn process(&mut self, frame: Frame) -> Frame {
        let (mut has_sps, mut has_pps) = (false, false);
        for nal in nal_units(&frame.data) {
            match nal_type(&nal) {
                NAL_SPS => {
                    has_sps = true;
                    self.sps = Some(nal);
                }
                NAL_PPS => {
                    has_pps = true;
                    self.pps = Some(nal);
                }
                _ => {}
            }
        }
        if !frame.keyframe || (has_sps && has_pps) {
            return frame;
        }
        let mut data = BytesMut::new();
        for (present, cached) in [(has_sps, &self.sps), (has_pps, &self.pps)] {
            if let (false, Some(nal)) = (present, cached) {
                put_nal(&mut data, nal);
            }
        }
        data.extend_from_slice(&frame.data);
        Frame {
            data: data.freeze(),
            ..frame
        }
    }

    /// Both parameter sets, once each has been seen.
    pub fn current(&self) -> Option<ParameterSets> {
        Some(ParameterSets {
            sps: self.sps.clone()?,
            pps: self.pps.clone()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1e, 0x8c, 0x8d];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    // first_mb_in_slice = 0 (top bit set) for both pictures.
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0x41, 0x9a, 0x21, 0x00];

    fn stream(nals: &[&[u8]]) -> Bytes {
        let mut out = BytesMut::new();
        for nal in nals {
            put_nal(&mut out, nal);
        }
        out.freeze()
    }

    #[test]
    fn splits_nal_units_with_three_and_four_byte_start_codes() {
        let data = Bytes::from_static(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65]);
        let types: Vec<u8> = nal_units(&data).map(|nal| nal_type(&nal)).collect();
        assert_eq!(types, [NAL_SPS, NAL_PPS, NAL_IDR]);
    }

    #[test]
    fn groups_access_units() {
        let mut splitter = AccessUnitSplitter::default();
        let mut units = Vec::new();
        for nal in [SPS, PPS, IDR, P, P, SPS, PPS, IDR] {
            units.extend(splitter.push(nal));
        }
        units.extend(splitter.finish());
        let keyframes: Vec<bool> = units.iter().map(|(_, key)| *key).collect();
        assert_eq!(keyframes, [true, false, false, true]);
        assert_eq!(units[0].0, stream(&[SPS, PPS, IDR]));
        assert_eq!(units[1].0, stream(&[P]));
        assert_eq!(units[3].0, stream(&[SPS, PPS, IDR]));
    }

    #[test]
    fn a_second_slice_of_the_same_picture_stays_in_its_unit() {
        let mut splitter = AccessUnitSplitter::default();
        // first_mb_in_slice != 0: the top bit of the slice header is clear.
        let second_slice: &[u8] = &[0x65, 0x40, 0x11];
        assert!(splitter.push(IDR).is_none());
        assert!(splitter.push(second_slice).is_none());
        let (data, key) = splitter.push(P).unwrap();
        assert!(key);
        assert_eq!(data, stream(&[IDR, second_slice]));
        assert!(splitter.finish().is_some());
        assert!(splitter.finish().is_none());
    }

    #[test]
    fn prepends_cached_parameter_sets_to_a_bare_idr() {
        let mut cache = ParameterSetCache::default();
        assert!(cache.current().is_none());
        let first = Frame {
            data: stream(&[SPS, PPS, IDR]),
            pts: 0,
            keyframe: true,
        };
        assert_eq!(cache.process(first.clone()), first);
        let inter = Frame {
            data: stream(&[P]),
            pts: 3600,
            keyframe: false,
        };
        assert_eq!(cache.process(inter.clone()), inter);
        let bare = Frame {
            data: stream(&[IDR]),
            pts: 7200,
            keyframe: true,
        };
        let fixed = cache.process(bare);
        assert_eq!(fixed.data, stream(&[SPS, PPS, IDR]));
        assert_eq!(fixed.pts, 7200);
        assert_eq!(cache.current().unwrap().annex_b(), stream(&[SPS, PPS]));
    }

    #[test]
    fn sdp_parameters_come_from_the_sps() {
        let sets = ParameterSets {
            sps: Bytes::from_static(SPS),
            pps: Bytes::from_static(PPS),
        };
        let fmtp = sets.sdp_parameters().unwrap().to_string();
        assert_eq!(
            fmtp,
            "packetization-mode=1;profile-level-id=42C01E;sprop-parameter-sets=Z0LAHoyN,aM48gA=="
        );
    }
}
