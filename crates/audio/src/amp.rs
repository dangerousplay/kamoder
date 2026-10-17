//! Who switches the speaker amplifier (D-A6).
//!
//! On the AV100 the amp enable is GPIO 37, active low (live-tested
//! 2026-10-17: sound only with the line low). The `ak_pcm` driver also
//! drives it, wrongly: it ignores the DT polarity, so `SET_SPEAKER 1` and
//! `SET_SOURCE 4` (route DAC to the output) set it high, i.e. muted, and
//! closing the node sets it low. The amp must therefore be switched from
//! userspace *after* the node is set up, and switched off again *after* it
//! is closed (see `Speaker::close`). `--amp` picks who does it:
//!
//! - [`Amp::Lights`] (default): `SetMute` on the lights socket, so `lights`
//!   stays the only userspace GPIO owner and its profile carries the
//!   polarity;
//! - [`Amp::Driver`]: the backend's `set_speaker`, i.e. ioctl `SET_SPEAKER`
//!   on the open playback node, for boards whose driver gets it right;
//! - [`Amp::None`]: the board has no switchable amp.
//!
//! The runtime turns the amp on when a talk starts (or frames resume) and
//! off a tail after the last frame (AUD-20); a failure is logged and
//! counted, and the talk goes on.

use crate::backend::Backend;

/// The amplifier switch; see the module docs.
#[derive(Debug, Clone)]
pub enum Amp {
    Driver,
    Lights(lights_proto::Client),
    None,
}

impl Amp {
    /// Name for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Amp::Driver => "driver",
            Amp::Lights(_) => "lights",
            Amp::None => "none",
        }
    }

    /// Switches the amp; `Err` carries the reason for the log.
    pub async fn set(&self, backend: &dyn Backend, on: bool) -> Result<(), String> {
        match self {
            Amp::Driver => backend.set_speaker(on).map_err(|e| e.to_string()),
            Amp::Lights(client) => match client.set_mute(!on).await {
                Ok(lights_proto::Response::State { .. }) => Ok(()),
                Ok(lights_proto::Response::Error { code, message }) => Err(format!(
                    "lights refused set_mute ({}): {message}",
                    code.as_str()
                )),
                Err(error) => Err(error.to_string()),
            },
            Amp::None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBackend;
    use crate::test_support::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[tokio::test(flavor = "current_thread")]
    async fn driver_and_none() {
        let fake = FakeBackend::default();
        Amp::Driver.set(&fake, true).await.unwrap();
        Amp::None.set(&fake, false).await.unwrap();
        Amp::Driver.set(&fake, false).await.unwrap();
        assert_eq!(fake.speaker_log(), [true, false]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lights_sends_set_mute() {
        let dir = TempDir::new("amp");
        let path = dir.path().join("lights.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut lines = Vec::new();
            for reply in [
                serde_json::json!({"result":"state","state":lights_proto::State::default()}),
                serde_json::json!({"result":"error","code":"invalid_argument","message":"no amp"}),
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                lines.push(line);
                let mut out = serde_json::to_vec(&reply).unwrap();
                out.push(b'\n');
                reader.get_mut().write_all(&out).await.unwrap();
            }
            lines
        });
        let amp = Amp::Lights(lights_proto::Client::new(&path));
        let fake = FakeBackend::default();
        amp.set(&fake, true).await.unwrap();
        let err = amp.set(&fake, false).await.unwrap_err();
        assert!(err.contains("no amp"), "{err}");
        let lines = server.await.unwrap();
        assert_eq!(lines[0].trim(), r#"{"op":"set_mute","muted":false}"#);
        assert_eq!(lines[1].trim(), r#"{"op":"set_mute","muted":true}"#);
        assert!(fake.speaker_log().is_empty());
    }
}
