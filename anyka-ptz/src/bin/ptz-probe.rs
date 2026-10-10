//! On-device check of the PTZ backend: opens it the way `anyka-onvif` will
//! and runs a few commands, printing the position after each.
//!
//! ```text
//! ptz-probe [--ptz auto|sdk|preset-motor|sim] [--presets <file>] status
//! ptz-probe goto <pan_deg> <tilt_deg>
//! ptz-probe move <pan> <tilt> <seconds>     # normalised speeds in [-1, 1]
//! ptz-probe home                            # re-home against the end stops
//! ```
//!
//! `--presets` also places the AV100 last-position file next to it; without
//! it the preset_motor backend has no saved position and homes on every run
//! unless the already-loaded driver knows where it is.
//!
//! Also the build gate proving the crate links against `libplat_drv`.

use anyka_ptz::{open_auto, BackendKind, OpenOptions, PtzBackend};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

fn usage() -> ExitCode {
    eprintln!(
        "usage: ptz-probe [--ptz auto|sdk|preset-motor|sim] [--presets <file>] status\n\
         \x20      ptz-probe [options] goto <pan_deg> <tilt_deg>\n\
         \x20      ptz-probe [options] move <pan> <tilt> <seconds>\n\
         \x20      ptz-probe [options] home"
    );
    ExitCode::from(2)
}

fn num(arg: Option<&String>) -> Option<f32> {
    arg?.parse().ok()
}

fn print(ptz: &dyn PtzBackend) {
    let p = ptz.position();
    println!(
        "pan {:7.1}°  tilt {:6.1}°  {}",
        p.pan_deg,
        p.tilt_deg,
        if p.moving { "moving" } else { "idle" }
    );
}

/// Prints the position every 250 ms until idle or `timeout`.
fn watch(ptz: &dyn PtzBackend, timeout: Duration) {
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        print(ptz);
        if !ptz.position().moving || start.elapsed() > timeout {
            break;
        }
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut kind = BackendKind::Auto;
    let mut options = OpenOptions::default();
    loop {
        match args.first().map(String::as_str) {
            Some("--ptz") => match args.get(1).map(|s| s.parse()) {
                Some(Ok(k)) => kind = k,
                _ => return usage(),
            },
            Some("--presets") => match args.get(1) {
                Some(path) => options.presets_path = Some(PathBuf::from(path)),
                None => return usage(),
            },
            _ => break,
        }
        args.drain(..2);
    }

    let ptz = match open_auto(kind, &options) {
        Ok(Some(ptz)) => ptz,
        Ok(None) => {
            eprintln!("no PTZ hardware found");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot open PTZ: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("backend: {}, limits: {:?}", ptz.name(), ptz.limits());
    print(ptz.as_ref());

    let result = match args.first().map(String::as_str) {
        Some("status") => Ok(()),
        Some("goto") => match (num(args.get(1)), num(args.get(2))) {
            (Some(pan), Some(tilt)) => ptz
                .goto(pan, tilt)
                .map(|()| watch(ptz.as_ref(), Duration::from_secs(60))),
            _ => return usage(),
        },
        Some("move") => match (num(args.get(1)), num(args.get(2)), num(args.get(3))) {
            (Some(pan), Some(tilt), Some(secs)) if secs >= 0.0 => ptz
                .continuous_move(pan, tilt)
                .and_then(|()| {
                    std::thread::sleep(Duration::from_secs_f32(secs));
                    ptz.stop()
                })
                .map(|()| watch(ptz.as_ref(), Duration::from_secs(10))),
            _ => return usage(),
        },
        Some("home") => ptz
            .home()
            .map(|()| watch(ptz.as_ref(), Duration::from_secs(30))),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
