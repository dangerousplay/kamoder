//! One source, many clients.
//!
//! A single task pulls frames from the [`VideoSource`] and publishes them on a
//! bounded `broadcast` channel. Each playing session subscribes and runs its
//! own packetizer, so a slow client never holds up the source or the others:
//! when it falls more than [`FRAME_BUFFER`] frames behind, `broadcast` tells
//! it how many it missed and it waits for the next IDR (see `session.rs`).
//! Memory is bounded by the channel, not by the number of clients.

use crate::h264::{Frame, ParameterSetCache, ParameterSets};
use crate::source::{self, VideoSource};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// Frames held for the slowest subscriber: about a third of a second at
/// 25 fps. More only delays the skip to the next IDR.
pub const FRAME_BUFFER: usize = 8;

/// The subscriber side, cloned into every request handler.
#[derive(Clone)]
pub struct Stream {
    frames: broadcast::Sender<Frame>,
    parameters: watch::Receiver<Option<ParameterSets>>,
}

impl Stream {
    /// Starts the pump task. The returned handle resolves when the source
    /// ends or fails, which the caller treats as fatal.
    pub fn spawn<S: VideoSource>(
        mut source: S,
    ) -> (Self, tokio::task::JoinHandle<Result<(), source::Error>>) {
        let (frames, _) = broadcast::channel(FRAME_BUFFER);
        let (publish, parameters) = watch::channel(None);
        let sender = frames.clone();
        let pump = tokio::spawn(async move {
            let mut cache = ParameterSetCache::default();
            while let Some(frame) = source.next_frame().await? {
                let frame = cache.process(frame);
                let current = cache.current();
                publish.send_if_modified(|sets| {
                    let changed = *sets != current;
                    if changed {
                        tracing::info!("H.264 parameter sets updated");
                        *sets = current;
                    }
                    changed
                });
                // No subscribers is the normal idle state, not an error.
                let _ = sender.send(frame);
            }
            Ok(())
        });
        (Self { frames, parameters }, pump)
    }

    /// A receiver that sees every frame published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.frames.subscribe()
    }

    /// The stream's SPS/PPS, waiting up to `timeout` for the source to
    /// produce them (a DESCRIBE right after start-up).
    pub async fn parameters(&self, timeout: Duration) -> Option<ParameterSets> {
        let mut parameters = self.parameters.clone();
        let ready = tokio::time::timeout(timeout, parameters.wait_for(Option::is_some)).await;
        match ready {
            Ok(Ok(sets)) => sets.clone(),
            _ => None,
        }
    }
}
