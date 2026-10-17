//! Runtime and socket tests over [`FakeBackend`].

use super::*;
use crate::fake::{FakeBackend, Signal};
use crate::server;
use crate::test_support::{wait_for, TempDir};
use audio_proto::{Client, ClientError};

fn device(id: &str, node: &str, gain: Option<u32>, range: [u32; 2]) -> board::AudioDevice {
    board::AudioDevice {
        id: id.into(),
        name: format!("Device {id}"),
        device: PathBuf::from(node),
        gain,
        gain_range: range,
    }
}

/// One mic (`mic0`, node `mic`, gain 3 of 0..=7) and one speaker (`spk0`,
/// node `spk`, volume 4 of 0..=6).
pub(crate) fn board(rate: u32) -> board::Audio {
    board::Audio {
        backend: "fake".into(),
        rate,
        inputs: vec![device("mic0", "mic", Some(3), [0, 7])],
        outputs: vec![device("spk0", "spk", Some(4), [0, 6])],
    }
}

/// 20 ms periods at 16 kHz and short timings, so tests run in real time.
fn fast() -> Settings {
    Settings {
        period_bytes: 640,
        capture_idle: Duration::from_millis(200),
        speaker_tail: Duration::from_millis(100),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
    }
}

fn start(fake: &FakeBackend, settings: Settings, store: Option<PathBuf>) -> Runtime {
    Runtime::start(
        &board(16000),
        Arc::new(fake.clone()),
        Amp::Driver,
        settings,
        store,
    )
    .unwrap()
}

fn device_status(runtime: &Runtime, id: &str) -> Device {
    runtime
        .status()
        .into_iter()
        .find(|d| d.id == id)
        .expect("device listed")
}

fn rms(x: &[i16]) -> f64 {
    (x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn tone() -> FakeBackend {
    FakeBackend::new(Signal::Sine {
        hz: 1000.0,
        amplitude: 8000.0,
    })
}

#[test]
fn bad_configuration_is_refused() {
    let fake = FakeBackend::default();
    let err = Runtime::start(
        &board(11025),
        Arc::new(fake.clone()),
        Amp::None,
        fast(),
        None,
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("11025"), "{err}");
    let odd = Settings {
        period_bytes: 641,
        ..fast()
    };
    let err = Runtime::start(&board(16000), Arc::new(fake), Amp::None, odd, None)
        .err()
        .unwrap();
    assert!(matches!(err, Error::Period { bytes: 641 }));
}

#[tokio::test(flavor = "current_thread")]
async fn status_lists_the_board_devices() {
    let runtime = start(&FakeBackend::default(), fast(), None);
    let status = runtime.status();
    assert_eq!(
        status,
        vec![
            Device {
                id: "mic0".into(),
                kind: DeviceKind::Input,
                name: "Device mic0".into(),
                state: DeviceState::Idle,
                rate: 8000,
                gain: Some(3),
                gain_range: [0, 7],
                error: None,
            },
            Device {
                id: "spk0".into(),
                kind: DeviceKind::Output,
                name: "Device spk0".into(),
                state: DeviceState::Idle,
                rate: 8000,
                gain: Some(4),
                gain_range: [0, 6],
                error: None,
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn capture_fans_out_to_two_clients() {
    let dir = TempDir::new("fanout");
    let path = dir.path().join("audio.sock");
    let fake = tone();
    let runtime = start(&fake, fast(), None);
    let _server = server::serve(&path, runtime.clone()).unwrap();

    let client = Client::new(&path);
    let mut a = client.capture().await.unwrap();
    let mut b = client.capture().await.unwrap();
    assert_eq!(a.format(), audio_proto::StreamFormat::DEFAULT);
    let mut seen = [Vec::new(), Vec::new()];
    for _ in 0..10 {
        for (stream, seen) in [&mut a, &mut b].into_iter().zip(&mut seen) {
            let frame = stream.next_frame().await.unwrap().expect("a frame");
            assert_eq!(frame.samples.len(), 160);
            // 1 kHz at 8000 peak, through the decimator unharmed.
            let level = rms(&frame.samples);
            assert!((5000.0..6000.0).contains(&level), "rms {level}");
            seen.push(frame.ts_ms);
        }
    }
    for ts in &seen {
        assert!(ts.windows(2).all(|w| w[1] == w[0] + 20), "{ts:?}");
        let now = monotonic_ms();
        assert!(ts[0] <= now && now - ts[0] < 5000, "monotonic clock");
    }
    // Both got the same capture, not one each.
    assert!(seen[0].iter().any(|ts| seen[1].contains(ts)));
    assert_eq!(fake.capture_opens(), 1);
    assert_eq!(device_status(&runtime, "mic0").state, DeviceState::Active);
}

#[tokio::test(flavor = "current_thread")]
async fn capture_stops_after_idle() {
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    assert!(!fake.capturing(), "nothing captured without subscribers");

    let mut sub = runtime.subscribe().unwrap();
    assert!(sub.recv().await.is_some());
    drop(sub);
    let left = std::time::Instant::now();
    // Back within the idle time: the same capture continues.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fake.capturing());
    let mut sub = runtime.subscribe().unwrap();
    assert!(sub.recv().await.is_some());
    drop(sub);
    let left_again = std::time::Instant::now();
    assert!(left_again - left < Duration::from_millis(200));

    wait_for("capture to stop", || !fake.capturing()).await;
    assert!(left_again.elapsed() >= Duration::from_millis(200));
    assert_eq!(fake.capture_opens(), 1);
    wait_for("idle state", || {
        device_status(&runtime, "mic0").state == DeviceState::Idle
    })
    .await;

    // And restarts for the next subscriber.
    let mut sub = runtime.subscribe().unwrap();
    assert!(sub.recv().await.is_some());
    assert_eq!(fake.capture_opens(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn second_talker_is_busy() {
    let dir = TempDir::new("busy");
    let path = dir.path().join("audio.sock");
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    let _server = server::serve(&path, runtime.clone()).unwrap();

    let client = Client::new(&path);
    let mut web = client.play(Talker::Web).await.unwrap();
    let error = client.play(Talker::Onvif).await.unwrap_err();
    match error {
        ClientError::Busy { message } => assert_eq!(message, "speaker held by web"),
        other => panic!("{other}"),
    }
    assert!(matches!(
        runtime.talk(Talker::Other).await,
        Err(Refused {
            code: ErrorCode::Busy,
            ..
        })
    ));
    web.send(&Frame {
        ts_ms: 0,
        samples: vec![1000; 160],
    })
    .await
    .unwrap();
    web.finish().await.unwrap();

    // Released when the first talker leaves.
    wait_for("speaker released", || lock(&runtime.inner.talker).is_none()).await;
    let onvif = client.play(Talker::Onvif).await.unwrap();
    drop(onvif);
}

#[tokio::test(flavor = "current_thread")]
async fn talk_plays_resampled_audio_and_fills_gaps() {
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    let talk = runtime.talk(Talker::Web).await.unwrap();
    assert_eq!(talk.talker(), Talker::Web);
    assert!(fake.speaker_on(), "amp on at talk start");
    assert_eq!(device_status(&runtime, "spk0").state, DeviceState::Active);
    // The board's volume, applied at open.
    assert_eq!(fake.gains(), [(PathBuf::from("spk"), 4)]);

    let tone: Vec<i16> = (0..160 * 25)
        .map(|n| (8000.0 * (2.0 * std::f64::consts::PI * f64::from(n) / 8.0).sin()) as i16)
        .collect();
    for chunk in tone.chunks(160) {
        talk.play(chunk.to_vec());
    }
    // 25 × 20 ms at 8 kHz = 8000 samples at 16 kHz.
    // Then silence keeps the driver fed while the talker is quiet.
    wait_for("the tone, then silence", || {
        let played = fake.played();
        let Some(start) = played.iter().position(|&s| s.abs() > 1000) else {
            return false;
        };
        played.len() >= start + 8000 + 2 * 320
            && played[played.len() - 320..].iter().all(|&s| s == 0)
    })
    .await;
    let after = fake.played();
    assert_eq!(after.len() % 320, 0, "whole 20 ms periods");
    let start = after.iter().position(|&s| s.abs() > 1000).unwrap();
    let tone_rms = rms(&after[start + 200..start + 7600]);
    assert!((5000.0..6000.0).contains(&tone_rms), "rms {tone_rms}");
    drop(talk);
    wait_for("playback closed", || !fake.playing()).await;
    assert!(!fake.speaker_on());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speaker_tail_timing() {
    let fake = FakeBackend::default();
    let settings = Settings {
        speaker_tail: Duration::from_secs(1),
        ..fast()
    };
    let runtime = start(&fake, settings, None);

    // Talk, one frame, hang up: on for the tail after the hang-up.
    let talk = runtime.talk(Talker::Web).await.unwrap();
    assert_eq!(fake.speaker_log(), [true]);
    talk.play(vec![0; 160]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(talk);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(fake.speaker_on(), "still on 900 ms after the talk ended");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.speaker_log(), [true, false], "off 1 s after");

    // A talker that stalls: off 1 s after its last frame, on again with
    // the next one, without closing the node.
    let talk = runtime.talk(Talker::Onvif).await.unwrap();
    let opens = fake.playback_opens();
    talk.play(vec![0; 160]);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!fake.speaker_on(), "stalled talker: amp off");
    assert_eq!(fake.speaker_log(), [true, false, true, false]);
    talk.play(vec![0; 160]);
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(fake.speaker_on(), "frames resumed: amp on");
    assert_eq!(fake.playback_opens(), opens, "node kept open");

    // A new talk within the tail keeps the amp on throughout.
    drop(talk);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let talk = runtime.talk(Talker::Web).await.unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(fake.speaker_on());
    assert_eq!(fake.speaker_log(), [true, false, true, false, true]);
    drop(talk);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!fake.speaker_on());
}

#[tokio::test(flavor = "current_thread")]
async fn set_persists_and_rejects_bad_values() {
    let dir = TempDir::new("set");
    let path = dir.path().join("audio.sock");
    let store = dir.path().join("audio.json");
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), Some(store.clone()));
    let server = server::serve(&path, runtime.clone()).unwrap();
    let client = Client::new(&path);

    client.set("mic0", 5).await.unwrap();
    assert_eq!(store::load(&store).gains, [("mic0".to_owned(), 5)].into());
    for (device, gain, code) in [
        ("mic0", 8, ErrorCode::OutOfRange),
        ("spk0", 7, ErrorCode::OutOfRange),
        ("nope", 1, ErrorCode::UnknownDevice),
    ] {
        match client.set(device, gain).await.unwrap_err() {
            ClientError::Rejected { code: got, message } => {
                assert_eq!(got, code, "{device} {gain}: {message}")
            }
            other => panic!("{other}"),
        }
    }
    let status = client.status().await.unwrap();
    assert_eq!(status[0].gain, Some(5));
    assert_eq!(status[1].gain, Some(4), "untouched: the board's");

    // Applied at open, and live while open.
    let mut sub = runtime.subscribe().unwrap();
    sub.recv().await.unwrap();
    assert_eq!(fake.gains(), [(PathBuf::from("mic"), 5)]);
    client.set("mic0", 6).await.unwrap();
    wait_for("live gain", || {
        fake.gains().last() == Some(&(PathBuf::from("mic"), 6))
    })
    .await;
    drop(sub);
    drop(server);

    // A restart applies the saved values; a saved value the board no longer
    // allows falls back to the board's.
    let mut saved = store::load(&store);
    saved.gains.insert("spk0".into(), 99);
    saved.gains.insert("gone".into(), 1);
    store::save(&store, &saved).unwrap();
    let again = start(&FakeBackend::default(), fast(), Some(store.clone()));
    assert_eq!(device_status(&again, "mic0").gain, Some(6));
    assert_eq!(device_status(&again, "spk0").gain, Some(4));
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_save_changes_nothing() {
    let dir = TempDir::new("set-fail");
    let store = dir.path().join("blocked");
    std::fs::create_dir_all(store.join("child")).unwrap();
    let runtime = start(&FakeBackend::default(), fast(), Some(store));
    let refused = runtime.set("mic0", 1).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Unavailable);
    assert_eq!(device_status(&runtime, "mic0").gain, Some(3));
    // Without a store, changes stay in memory.
    let memory = start(&FakeBackend::default(), fast(), None);
    memory.set("mic0", 1).unwrap();
    assert_eq!(device_status(&memory, "mic0").gain, Some(1));
}

#[tokio::test(flavor = "current_thread")]
async fn capture_errors_show_and_recover() {
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    fake.fail_capture_opens(2);
    let mut sub = runtime.subscribe().unwrap();
    wait_for("error state", || {
        device_status(&runtime, "mic0").state == DeviceState::Error
    })
    .await;
    let mic = device_status(&runtime, "mic0");
    assert!(mic.error.unwrap().contains("capture open failed"));

    // Retried with backoff until it opens; the subscriber just waits.
    assert!(sub.recv().await.is_some());
    let mic = device_status(&runtime, "mic0");
    assert_eq!((mic.state, mic.error), (DeviceState::Active, None));
    assert_eq!(fake.capture_opens(), 1);

    // A read failure mid-stream: reopened, frames continue.
    fake.fail_reads(1);
    wait_for("reopen after a read error", || fake.capture_opens() == 2).await;
    assert!(sub.recv().await.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn playback_errors_show_and_recover() {
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    fake.fail_playback_opens(1);
    let refused = runtime.talk(Talker::Web).await.err().unwrap();
    assert_eq!(refused.code, ErrorCode::Unavailable);
    assert!(
        refused.message.contains("playback open failed"),
        "{refused}"
    );
    let spk = device_status(&runtime, "spk0");
    assert_eq!(spk.state, DeviceState::Error);
    // The speaker is not held by a refused talk.
    assert!(lock(&runtime.inner.talker).is_none());
    // Within the backoff: refused without touching the driver.
    let refused = runtime.talk(Talker::Web).await.err().unwrap();
    assert!(refused.message.contains("retrying"), "{refused}");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let talk = runtime.talk(Talker::Web).await.unwrap();
    assert_eq!(device_status(&runtime, "spk0").state, DeviceState::Active);

    // A write failure: error state, then a reopen on later frames.
    fake.fail_writes(1);
    wait_for("write error", || {
        device_status(&runtime, "spk0").state == DeviceState::Error
    })
    .await;
    wait_for("reopen", || {
        talk.play(vec![0; 160]);
        fake.playback_opens() == 2
    })
    .await;
    assert_eq!(device_status(&runtime, "spk0").state, DeviceState::Active);
}

#[tokio::test(flavor = "current_thread")]
async fn boards_without_a_device_refuse_it() {
    let mut audio = board(8000);
    audio.inputs.clear();
    let runtime = Runtime::start(
        &audio,
        Arc::new(FakeBackend::default()),
        Amp::None,
        fast(),
        None,
    )
    .unwrap();
    assert_eq!(
        runtime.subscribe().err().unwrap().code,
        ErrorCode::Unavailable
    );
    let talk = runtime.talk(Talker::Other).await.unwrap();
    drop(talk);

    let mut audio = board(8000);
    audio.outputs.clear();
    let runtime = Runtime::start(
        &audio,
        Arc::new(FakeBackend::default()),
        Amp::None,
        fast(),
        None,
    )
    .unwrap();
    assert_eq!(
        runtime.talk(Talker::Web).await.err().unwrap().code,
        ErrorCode::Unavailable
    );
    // 8 kHz hardware: frames pass straight through.
    let mut sub = runtime.subscribe().unwrap();
    assert_eq!(sub.recv().await.unwrap().samples.len(), 160);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_turns_the_amp_off() {
    let fake = FakeBackend::default();
    let runtime = start(&fake, fast(), None);
    let talk = runtime.talk(Talker::Web).await.unwrap();
    assert!(fake.speaker_on());
    runtime.shutdown().await;
    assert!(!fake.speaker_on());
    drop(talk);
}
