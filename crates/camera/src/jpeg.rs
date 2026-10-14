//! NV12 → JPEG in software (`jpeg-encoder`, pure Rust). A 640×360 picture
//! takes about 65 ms on the 400 MHz ARM926, which is what the preview needs.

use crate::backend::Frame;
use jpeg_encoder::{Encoder, EncodingError, ImageBuffer, JpegColorType, SamplingFactor};

struct Nv12<'a>(&'a Frame);

impl ImageBuffer for Nv12<'_> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.0.width as u16
    }

    fn height(&self) -> u16 {
        self.0.height as u16
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let w = self.0.width as usize;
        let h = self.0.height as usize;
        let y = usize::from(y);
        let luma = &self.0.data[y * w..][..w];
        let chroma = &self.0.data[w * h + (y / 2) * w..][..w];
        buffers[0].extend_from_slice(luma);
        for pair in chroma.as_chunks::<2>().0 {
            buffers[1].extend_from_slice(&[pair[0], pair[0]]);
            buffers[2].extend_from_slice(&[pair[1], pair[1]]);
        }
    }
}

/// Encodes `frame` at `quality` (1..=100) with 4:2:0 chroma.
pub fn encode(frame: &Frame, quality: u8) -> Result<Vec<u8>, EncodingError> {
    let mut out = Vec::with_capacity(frame.data.len() / 4);
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder.encode_image(Nv12(frame))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_grey_frame() {
        let (w, h) = (64u32, 32u32);
        let frame = Frame {
            width: w,
            height: h,
            data: vec![128; (w * h * 3 / 2) as usize],
            timestamp_ms: 1,
        };
        let jpeg = encode(&frame, 80).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xff, 0xd9]);
    }
}
