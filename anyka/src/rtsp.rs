//! The SDK's built-in RTSP server (`libapp_rtsp`).
//!
//! The server owns its own encoders: it is handed a [`VideoInput`] plus
//! per-channel encode settings and streams `rtsp://<ip>:554/<suffix>` by
//! itself. [`RtspServer`] keeps that global state tied to one value, so
//! `ak_rtsp_exit` always runs and the two channels cannot be started twice.

use crate::error::{check, Error, Result};
use crate::venc::{BitrateMode, OutputType};
use crate::vi::VideoInput;

/// Per-channel RTSP configuration.
#[derive(Debug, Clone)]
pub struct RtspChannelConfig {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    /// Ceiling for the rate controller, in kbps.
    pub max_kbps: i32,
    pub min_qp: i32,
    pub max_qp: i32,
    pub gop_len: i32,
    pub codec: OutputType,
    pub bitrate_mode: BitrateMode,
    /// URL path component, e.g. `vs0` for `rtsp://camera/vs0`.
    pub suffix: String,
}

impl Default for RtspChannelConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 25,
            max_kbps: 2000,
            min_qp: 20,
            max_qp: 51,
            gop_len: 2,
            codec: OutputType::H264,
            bitrate_mode: BitrateMode::Cbr,
            suffix: "vs0".into(),
        }
    }
}

/// Maximum length of the URL suffix, including the NUL the SDK writes.
const SUFFIX_CAPACITY: usize = anyka_sys::AK_RTSP_SUFFIX_NAME_LEN as usize;

/// A running RTSP server.
///
/// Both channels are started by [`RtspServer::start`] and stopped when this
/// value drops. It borrows the [`VideoInput`], because the SDK keeps the raw
/// handle internally for as long as the server runs.
#[derive(Debug)]
pub struct RtspServer<'vi> {
    _vi: &'vi VideoInput,
}

impl<'vi> RtspServer<'vi> {
    /// Initialises and starts the server for the main and sub channels.
    pub fn start(
        vi: &'vi VideoInput,
        main: &RtspChannelConfig,
        sub: &RtspChannelConfig,
    ) -> Result<Self> {
        let mut param = anyka_sys::rtsp_param::default();
        for (index, cfg) in [main, sub].into_iter().enumerate() {
            let chn = &mut param.rtsp_chn[index];
            chn.current_channel = index as anyka_sys::rtsp_channel::Type;
            chn.width = cfg.width;
            chn.height = cfg.height;
            chn.fps = cfg.fps;
            chn.max_kbps = cfg.max_kbps;
            chn.min_qp = cfg.min_qp;
            chn.max_qp = cfg.max_qp;
            chn.gop_len = cfg.gop_len;
            chn.video_enc_type = raw_codec(cfg.codec);
            chn.video_br_mode = raw_bitrate_mode(cfg.bitrate_mode);
            chn.vi_handle = vi.as_ptr();
            write_suffix(&mut chn.suffix_name, &cfg.suffix)?;
        }

        // SAFETY: `param` is fully initialised and only read during the call;
        // the `vi_handle` it carries stays valid because the returned server
        // borrows the `VideoInput`.
        let ret = unsafe { anyka_sys::ak_rtsp_init(&mut param) };
        check("ak_rtsp_init", ret)?;

        let server = Self { _vi: vi };
        for channel in 0..2 {
            // SAFETY: plain integer; the server was initialised above.
            let ret = unsafe { anyka_sys::ak_rtsp_start(channel) };
            // `server` is already live, so a failure here still tears the
            // server down through its `Drop`.
            check("ak_rtsp_start", ret)?;
        }
        Ok(server)
    }
}

impl Drop for RtspServer<'_> {
    fn drop(&mut self) {
        for channel in 0..2 {
            // SAFETY: plain integer; stopping a channel that never started is
            // a no-op in the SDK.
            unsafe { anyka_sys::ak_rtsp_stop(channel) };
        }
        // SAFETY: matches the successful `ak_rtsp_init` in `start`.
        unsafe { anyka_sys::ak_rtsp_exit() };
    }
}

/// Copies `suffix` into the SDK's fixed-size, NUL-terminated field.
///
/// The C demo uses `strcpy` into this 10-byte array; anything longer silently
/// smashes the next struct field, so it is rejected here instead.
fn write_suffix(field: &mut [std::os::raw::c_char], suffix: &str) -> Result<()> {
    let bytes = suffix.as_bytes();
    if bytes.len() >= SUFFIX_CAPACITY {
        return Err(Error::invalid(
            "ak_rtsp_init",
            format!("channel suffix must be shorter than {SUFFIX_CAPACITY} bytes"),
        ));
    }
    if bytes.contains(&0) {
        return Err(Error::invalid(
            "ak_rtsp_init",
            "channel suffix contains a NUL byte",
        ));
    }
    field.fill(0);
    for (slot, &byte) in field.iter_mut().zip(bytes) {
        *slot = byte as std::os::raw::c_char;
    }
    Ok(())
}

/// `rtsp_channel_cfg_t.video_enc_type` is a plain `int`, not the enum type.
fn raw_codec(codec: OutputType) -> std::os::raw::c_int {
    match codec {
        OutputType::H264 => anyka_sys::encode_output_type::H264_ENC_TYPE as std::os::raw::c_int,
        OutputType::Mjpeg => anyka_sys::encode_output_type::MJPEG_ENC_TYPE as std::os::raw::c_int,
        OutputType::H265 => anyka_sys::encode_output_type::HEVC_ENC_TYPE as std::os::raw::c_int,
    }
}

fn raw_bitrate_mode(mode: BitrateMode) -> anyka_sys::bitrate_ctrl_mode::Type {
    match mode {
        BitrateMode::Cbr => anyka_sys::bitrate_ctrl_mode::BR_MODE_CBR,
        BitrateMode::Vbr => anyka_sys::bitrate_ctrl_mode::BR_MODE_VBR,
    }
}
