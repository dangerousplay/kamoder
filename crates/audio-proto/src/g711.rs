//! ITU-T G.711 A-law and µ-law companding, the codecs RTSP and ONVIF speak.
//!
//! The audio socket carries linear S16LE (D-A3); consumers compand at their
//! edge: `rtsp` encodes A-law (PCMA, RTP payload type 8) for the camera's
//! track and decodes whatever an ONVIF backchannel client sends (PCMA or
//! PCMU). Both directions live here so the tables are written once.
//!
//! The arithmetic is the public-domain Sun Microsystems reference
//! implementation (`g711.c`), which is bit-exact with G.711 and with what
//! every RTP stack (ffmpeg, GStreamer, live555) produces:
//!
//! - **A-law** keeps the top 13 bits of a sample, splits the magnitude into
//!   8 segments of 16 steps, and XORs the result with `0x55` (even bits
//!   inverted, sign bit set for positive). There is no zero code: `0` encodes
//!   to `0xD5` and decodes to `+8`.
//! - **µ-law** keeps the top 14 bits, adds a bias of 33 so every segment
//!   starts on a power of two, clips at 8159, and inverts every bit. `0`
//!   encodes to `0xFF`; `0x7F` is the never-produced "negative zero".
//!
//! The per-sample functions are branch-light integer code with no tables and
//! no allocation; the slice helpers loop over them for a 20 ms frame.

/// Upper end of each A-law segment, on the 13-bit magnitude.
const ALAW_SEG_END: [i32; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

/// Upper end of each µ-law segment, on the biased 14-bit magnitude.
const ULAW_SEG_END: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];

/// µ-law bias (`0x84` on the 16-bit scale, 33 on the 14-bit one).
const ULAW_BIAS: i32 = 0x84;

/// Largest µ-law magnitude on the 14-bit scale before the bias is added.
const ULAW_CLIP: i32 = 8159;

/// Index of the first segment whose end is `>= value`, or 8 if none is.
fn segment(value: i32, ends: &[i32; 8]) -> i32 {
    // Eight compares at most; position() on a fixed array does not allocate.
    ends.iter().position(|&end| value <= end).unwrap_or(8) as i32
}

/// One linear sample to its A-law code.
pub fn alaw_encode(sample: i16) -> u8 {
    let mut pcm = i32::from(sample) >> 3;
    let mask = if pcm >= 0 {
        0xD5
    } else {
        pcm = -pcm - 1;
        0x55
    };
    let seg = segment(pcm, &ALAW_SEG_END);
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let mantissa = if seg < 2 { pcm >> 1 } else { pcm >> seg } & 0x0F;
    ((seg << 4) | mantissa) as u8 ^ mask
}

/// One A-law code to its linear sample (the middle of its quantisation step).
pub fn alaw_decode(code: u8) -> i16 {
    let code = i32::from(code ^ 0x55);
    let seg = (code & 0x70) >> 4;
    let mut t = (code & 0x0F) << 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    // Largest magnitude is 0x7E00 + 0x200 = 32256: fits i16 either sign.
    (if code & 0x80 != 0 { t } else { -t }) as i16
}

/// One linear sample to its µ-law code.
pub fn ulaw_encode(sample: i16) -> u8 {
    let mut pcm = i32::from(sample) >> 2;
    let mask = if pcm < 0 {
        pcm = -pcm;
        0x7F
    } else {
        0xFF
    };
    pcm = pcm.min(ULAW_CLIP) + (ULAW_BIAS >> 2);
    let seg = segment(pcm, &ULAW_SEG_END);
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let code = (seg << 4) | ((pcm >> (seg + 1)) & 0x0F);
    code as u8 ^ mask
}

/// One µ-law code to its linear sample (the middle of its quantisation step).
pub fn ulaw_decode(code: u8) -> i16 {
    let code = i32::from(!code);
    let t = (((code & 0x0F) << 3) + ULAW_BIAS) << ((code & 0x70) >> 4);
    // Largest magnitude is 32124: fits i16 either sign.
    (if code & 0x80 != 0 {
        ULAW_BIAS - t
    } else {
        t - ULAW_BIAS
    }) as i16
}

/// Encodes `pcm` into `out` as A-law. Panics if the lengths differ.
pub fn alaw_encode_into(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(pcm.len(), out.len(), "alaw_encode_into: length mismatch");
    for (o, &s) in out.iter_mut().zip(pcm) {
        *o = alaw_encode(s);
    }
}

/// Decodes A-law `codes` into `out`. Panics if the lengths differ.
pub fn alaw_decode_into(codes: &[u8], out: &mut [i16]) {
    assert_eq!(codes.len(), out.len(), "alaw_decode_into: length mismatch");
    for (o, &c) in out.iter_mut().zip(codes) {
        *o = alaw_decode(c);
    }
}

/// Encodes `pcm` into `out` as µ-law. Panics if the lengths differ.
pub fn ulaw_encode_into(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(pcm.len(), out.len(), "ulaw_encode_into: length mismatch");
    for (o, &s) in out.iter_mut().zip(pcm) {
        *o = ulaw_encode(s);
    }
}

/// Decodes µ-law `codes` into `out`. Panics if the lengths differ.
pub fn ulaw_decode_into(codes: &[u8], out: &mut [i16]) {
    assert_eq!(codes.len(), out.len(), "ulaw_decode_into: length mismatch");
    for (o, &c) in out.iter_mut().zip(codes) {
        *o = ulaw_decode(c);
    }
}

/// `pcm` as A-law, in a new buffer.
pub fn alaw_encode_vec(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| alaw_encode(s)).collect()
}

/// A-law `codes` as linear samples, in a new buffer.
pub fn alaw_decode_vec(codes: &[u8]) -> Vec<i16> {
    codes.iter().map(|&c| alaw_decode(c)).collect()
}

/// `pcm` as µ-law, in a new buffer.
pub fn ulaw_encode_vec(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| ulaw_encode(s)).collect()
}

/// µ-law `codes` as linear samples, in a new buffer.
pub fn ulaw_decode_vec(codes: &[u8]) -> Vec<i16> {
    codes.iter().map(|&c| ulaw_decode(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alaw_matches_reference_points() {
        // Values from the Sun reference / ITU-T G.711 tables.
        assert_eq!(alaw_encode(0), 0xD5);
        assert_eq!(alaw_encode(-1), 0x55);
        assert_eq!(alaw_encode(i16::MAX), 0xAA);
        assert_eq!(alaw_encode(i16::MIN), 0x2A);
        assert_eq!(alaw_decode(0xD5), 8);
        assert_eq!(alaw_decode(0x55), -8);
        assert_eq!(alaw_decode(0xAA), 32256);
        assert_eq!(alaw_decode(0x2A), -32256);
        assert_eq!(alaw_decode(0x80), 5504);
        assert_eq!(alaw_decode(0x00), -5504);
    }

    #[test]
    fn ulaw_matches_reference_points() {
        assert_eq!(ulaw_encode(0), 0xFF);
        assert_eq!(ulaw_encode(-1), 0x7E);
        assert_eq!(ulaw_decode(0x7E), -8);
        assert_eq!(ulaw_encode(i16::MAX), 0x80);
        assert_eq!(ulaw_encode(i16::MIN), 0x00);
        assert_eq!(ulaw_decode(0xFF), 0);
        assert_eq!(ulaw_decode(0x7F), 0);
        assert_eq!(ulaw_decode(0x80), 32124);
        assert_eq!(ulaw_decode(0x00), -32124);
    }

    #[test]
    fn every_alaw_code_survives_decode_then_encode() {
        for code in 0..=255u8 {
            assert_eq!(alaw_encode(alaw_decode(code)), code, "{code:#04x}");
        }
    }

    #[test]
    fn every_ulaw_code_survives_decode_then_encode() {
        for code in 0..=255u8 {
            // 0x7F is negative zero: it decodes to 0, which encodes as 0xFF.
            let want = if code == 0x7F { 0xFF } else { code };
            assert_eq!(ulaw_encode(ulaw_decode(code)), want, "{code:#04x}");
        }
    }

    #[test]
    fn encode_then_decode_error_is_within_one_step() {
        for s in i16::MIN..=i16::MAX {
            let x = i32::from(s);
            // A-law step is 16 in segments 0..=1 and 2^(seg+4) above, on the
            // 16-bit scale; half a step either way, plus the 3-bit truncation.
            let a = i32::from(alaw_decode(alaw_encode(s)));
            let step = (x.unsigned_abs() >> 4).max(16).next_power_of_two() as i32;
            assert!((a - x).abs() <= step, "alaw {s} -> {a}");
            // µ-law clips at ±32124; its step is 8..1024, about 1/16 to 1/32
            // of the biased magnitude.
            let u = i32::from(ulaw_decode(ulaw_encode(s)));
            let clipped = x.clamp(-32124, 32124);
            let biased = clipped.unsigned_abs() + ULAW_BIAS as u32;
            let step = (biased >> 4).next_power_of_two() as i32;
            assert!((u - clipped).abs() <= step, "ulaw {s} -> {u}");
            // Sign survives (A-law has no zero, so 0 may come back as ±8).
            if x.abs() > 16 {
                assert_eq!(a.signum(), x.signum(), "alaw sign {s}");
                assert_eq!(u.signum(), x.signum(), "ulaw sign {s}");
            }
        }
    }

    #[test]
    fn slice_helpers_match_the_scalar_functions() {
        let pcm: Vec<i16> = (0..160)
            .map(|i| ((i * 409) as i16).wrapping_mul(7))
            .collect();
        let mut codes = [0u8; 160];
        let mut back = [0i16; 160];

        alaw_encode_into(&pcm, &mut codes);
        assert_eq!(codes.to_vec(), alaw_encode_vec(&pcm));
        alaw_decode_into(&codes, &mut back);
        assert_eq!(back.to_vec(), alaw_decode_vec(&codes));
        assert!(pcm.iter().zip(&codes).all(|(&s, &c)| alaw_encode(s) == c));

        ulaw_encode_into(&pcm, &mut codes);
        assert_eq!(codes.to_vec(), ulaw_encode_vec(&pcm));
        ulaw_decode_into(&codes, &mut back);
        assert_eq!(back.to_vec(), ulaw_decode_vec(&codes));
        assert!(codes.iter().zip(&back).all(|(&c, &s)| ulaw_decode(c) == s));
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn slice_helpers_reject_mismatched_lengths() {
        alaw_encode_into(&[0; 4], &mut [0; 3]);
    }
}
