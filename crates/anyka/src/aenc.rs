//! Audio encoding (MP3, AAC, AMR, G.711, raw PCM).
//!
//! Like the video encoder there are two modes: single-frame
//! ([`AudioEncoder::encode_frame`]) and streaming
//! ([`AudioEncoder::request_stream`]).
//!
//! The streaming API is the awkward one in C: `ak_aenc_get_stream` appends
//! kernel-style intrusive list nodes to a `struct list_head` the caller must
//! have initialised itself, and each node has to be freed individually with
//! `ak_aenc_release_stream`. [`AudioStreamHandle::drain`] hides that: it walks
//! the list, copies each payload out and releases every node, so a caller can
//! never forget one or walk a list it forgot to initialise.

use crate::ai::{AudioFrame, AudioIn};
use crate::error::{check, Error, Result};
use std::ffi::c_void;

/// Audio codec, mirroring the subset of `enum ak_audio_type` the encoder
/// supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioType {
    Mp3,
    Amr,
    Aac,
    /// G.711 A-law.
    G711A,
    /// G.711 µ-law.
    G711U,
    /// Uncompressed 16-bit PCM.
    Pcm,
}

impl AudioType {
    fn as_raw(self) -> anyka_sys::ak_audio_type::Type {
        match self {
            AudioType::Mp3 => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_MP3,
            AudioType::Amr => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_AMR,
            AudioType::Aac => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_AAC,
            AudioType::G711A => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_PCM_ALAW,
            AudioType::G711U => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_PCM_ULAW,
            AudioType::Pcm => anyka_sys::ak_audio_type::AK_AUDIO_TYPE_PCM,
        }
    }

    /// Parses the spelling the C demos accept on the command line.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mp3" => Some(AudioType::Mp3),
            "amr" => Some(AudioType::Amr),
            "aac" => Some(AudioType::Aac),
            "g711a" => Some(AudioType::G711A),
            "g711u" => Some(AudioType::G711U),
            "pcm" => Some(AudioType::Pcm),
            _ => None,
        }
    }

    /// Conventional file extension for this codec.
    pub fn extension(self) -> &'static str {
        match self {
            AudioType::Mp3 => "mp3",
            AudioType::Amr => "amr",
            AudioType::Aac => "aac",
            AudioType::G711A => "g711a",
            AudioType::G711U => "g711u",
            AudioType::Pcm => "pcm",
        }
    }

    /// Magic bytes a standalone file of this codec needs up front.
    pub fn file_header(self) -> &'static [u8] {
        match self {
            AudioType::Amr => b"#!AMR\n",
            _ => b"",
        }
    }

    /// Frame interval the codec needs, in milliseconds.
    ///
    /// Each codec has a fixed samples-per-frame count, so the ADC has to hand
    /// over exactly that much audio per frame or the encoder stalls.
    pub fn frame_interval_ms(self, sample_rate: u32) -> i32 {
        let rate = sample_rate.max(1) as i32;
        match self {
            // AAC consumes 1024 samples per frame.
            AudioType::Aac => (1024 * 1000) / rate,
            AudioType::Amr => crate::ai::AMR_FRAME_INTERVAL_MS,
            // MP3 uses a short frame below 32 kHz and a long one above.
            AudioType::Mp3 if (8000..=24000).contains(&rate) => 576 * 1000 / rate,
            AudioType::Mp3 => 1152 * 1000 / rate,
            _ => crate::ai::DEFAULT_FRAME_INTERVAL_MS,
        }
    }
}

/// Audio encoder configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioEncodeParam {
    pub codec: AudioType,
    pub sample_rate: u32,
    pub channels: u32,
}

impl AudioEncodeParam {
    fn to_raw(self) -> anyka_sys::audio_param {
        anyka_sys::audio_param {
            type_: self.codec.as_raw(),
            sample_rate: self.sample_rate,
            sample_bits: 16,
            channel_num: self.channels,
        }
    }
}

/// An open audio encoder.
#[derive(Debug)]
pub struct AudioEncoder {
    handle: *mut c_void,
    codec: AudioType,
}

// SAFETY: as with the other SDK handles, the vendor demos move the encoder
// handle between threads and the SDK locks the codec context internally.
unsafe impl Send for AudioEncoder {}
unsafe impl Sync for AudioEncoder {}

impl AudioEncoder {
    /// Opens an encoder for the given codec and PCM format.
    pub fn open(param: AudioEncodeParam) -> Result<Self> {
        let raw = param.to_raw();
        // SAFETY: `raw` is read-only for the call and fully initialised.
        let handle = unsafe { anyka_sys::ak_aenc_open(&raw) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_aenc_open"));
        }
        let encoder = Self {
            handle,
            codec: param.codec,
        };
        if param.codec == AudioType::Aac {
            // Without an ADTS header per frame the output is not playable as a
            // standalone file, which is what every caller here wants.
            encoder.set_keep_aac_frame_header(true)?;
        }
        Ok(encoder)
    }

    /// The codec this encoder was opened with.
    pub fn codec(&self) -> AudioType {
        self.codec
    }

    /// The raw `void *` handle, for SDK calls this crate does not wrap yet.
    pub fn as_ptr(&self) -> *mut c_void {
        self.handle
    }

    /// Keeps or strips the per-frame AAC (ADTS) header.
    pub fn set_keep_aac_frame_header(&self, keep: bool) -> Result<()> {
        let attr = anyka_sys::aenc_attr {
            aac_head: if keep {
                anyka_sys::aenc_aac_attr::AENC_AAC_SAVE_FRAME_HEAD
            } else {
                anyka_sys::aenc_aac_attr::AENC_AAC_CUT_FRAME_HEAD
            },
        };
        // SAFETY: valid handle; `attr` is read-only for the call.
        let ret = unsafe { anyka_sys::ak_aenc_set_attr(self.handle, &attr) };
        check("ak_aenc_set_attr", ret)
    }

    /// Encodes one PCM frame, appending the result to `out`.
    ///
    /// Unlike the video path the SDK writes into a buffer it keeps owning, so
    /// the bytes are copied and nothing needs freeing.
    pub fn encode_frame(&self, frame: &AudioFrame<'_>, out: &mut Vec<u8>) -> Result<usize> {
        let mut stream = anyka_sys::audio_stream::default();
        // SAFETY: the input frame is alive for the call and the SDK only reads
        // it; `stream` is a live out-parameter.
        let ret = unsafe { anyka_sys::ak_aenc_send_frame(self.handle, frame.raw(), &mut stream) };
        if ret < 0 {
            return Err(Error::from_sdk("ak_aenc_send_frame"));
        }
        if stream.data.is_null() || stream.len == 0 {
            return Ok(0);
        }
        let len = stream.len as usize;
        // SAFETY: on success `data[..len]` is initialised and stays valid until
        // the next call on this handle; it is copied out immediately.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(stream.data, len) });
        Ok(len)
    }

    /// Binds this encoder to an audio input and starts streaming.
    pub fn request_stream<'a>(&'a self, ai: &'a AudioIn) -> Result<AudioStreamHandle<'a>> {
        // SAFETY: both handles are valid and outlive the returned borrow.
        let handle = unsafe { anyka_sys::ak_aenc_request_stream(ai.as_ptr(), self.handle) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_aenc_request_stream"));
        }
        Ok(AudioStreamHandle {
            handle,
            _encoder: self,
        })
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        // SAFETY: valid handle; any stream borrowing it has already dropped.
        unsafe { anyka_sys::ak_aenc_close(self.handle) };
    }
}

/// A running audio encode session.
#[derive(Debug)]
pub struct AudioStreamHandle<'a> {
    handle: *mut c_void,
    _encoder: &'a AudioEncoder,
}

// SAFETY: `Send` only — all list manipulation happens through `&self` on one
// thread at a time, matching how the vendor demo uses it.
unsafe impl Send for AudioStreamHandle<'_> {}

impl AudioStreamHandle<'_> {
    /// Drains everything the encoder has queued.
    ///
    /// An empty result means nothing was ready yet — the SDK reports "queue
    /// empty" through the same failure path as a real error, so callers poll
    /// with a short sleep rather than treating it as fatal.
    ///
    /// Every SDK list node is released before this returns, including the ones
    /// whose payload was empty.
    pub fn drain(&self) -> Vec<EncodedAudio> {
        // `ak_aenc_get_stream` splices nodes onto a list head the caller owns,
        // so it must be self-linked before the call (the C API's
        // `INIT_LIST_HEAD`) and must not move afterwards, since the SDK stores
        // its address in the nodes it links in.
        let mut head = anyka_sys::list_head {
            next: std::ptr::null_mut(),
            prev: std::ptr::null_mut(),
        };
        let head_ptr: *mut anyka_sys::list_head = &mut head;
        // SAFETY: `head_ptr` points at the live local above; self-linking it
        // is exactly what `INIT_LIST_HEAD` does.
        unsafe {
            (*head_ptr).next = head_ptr;
            (*head_ptr).prev = head_ptr;
        }

        // SAFETY: valid handle; `head_ptr` is an initialised, non-moving list
        // head that stays alive for the whole function.
        unsafe { anyka_sys::ak_aenc_get_stream(self.handle, head_ptr) };

        // Walk and release whatever was linked in. This runs even when the call
        // above reported failure, because the SDK may have spliced some nodes
        // before giving up, and those still have to be freed.
        let mut out = Vec::new();
        // SAFETY: `head` is a well-formed circular list of `aenc_entry.list`
        // nodes owned by the SDK. The cursor advances to `next` *before* the
        // node is released, so releasing cannot invalidate the pointer still
        // needed for the next iteration.
        unsafe {
            let mut cursor = (*head_ptr).next;
            while !cursor.is_null() && !std::ptr::eq(cursor, head_ptr) {
                let next = (*cursor).next;
                let entry = (cursor as *mut u8)
                    .sub(std::mem::offset_of!(anyka_sys::aenc_entry, list))
                    as *mut anyka_sys::aenc_entry;
                let stream = &(*entry).stream;
                if !stream.data.is_null() && stream.len > 0 {
                    out.push(EncodedAudio {
                        data: std::slice::from_raw_parts(stream.data, stream.len as usize).to_vec(),
                        timestamp_ms: stream.ts,
                        sequence: stream.seq_no as u64,
                    });
                }
                anyka_sys::ak_aenc_release_stream(entry);
                cursor = next;
            }
        }

        out
    }
}

impl Drop for AudioStreamHandle<'_> {
    fn drop(&mut self) {
        // SAFETY: valid handle, cancelled exactly once.
        unsafe { anyka_sys::ak_aenc_cancel_stream(self.handle) };
    }
}

/// One encoded audio chunk, copied out of the SDK's list.
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub sequence: u64,
}
