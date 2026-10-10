//! Build, lint and packaging tasks for the Anyka camera workspace.
//!
//! Run through `./x`, which builds this for the host rather than for the
//! camera. Every task shells out to `cargo` in the workspace root, where
//! `.cargo/config.toml` already points the build at
//! `armv5te-unknown-linux-uclibceabi`.

mod flash;
mod gen_onvif;
mod itest;
mod tasks;

use eyre::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (task, rest) = args
        .split_first()
        .map(|(task, rest)| (task.as_str(), rest))
        .unwrap_or(("help", &[]));

    match task {
        "build" => tasks::build(rest),
        "check" => tasks::check(rest),
        "clippy" => tasks::clippy(rest),
        "fmt" => tasks::fmt(rest),
        "deny" => tasks::deny(rest),
        "test" => tasks::test(rest),
        "gen-onvif" => gen_onvif::gen_onvif(rest),
        "dist" => tasks::dist(rest),
        "sd" => tasks::sd(rest),
        "webui" => tasks::webui(rest),
        "flash" => flash::flash(rest),
        "itest" => itest::itest(rest),
        "qemu" => tasks::qemu(rest),
        "deploy" => tasks::deploy(rest),
        "ci" => tasks::ci(rest),
        "help" | "--help" | "-h" => {
            tasks::help();
            Ok(())
        }
        other => {
            eprintln!("xtask: unknown task {other:?}\n");
            tasks::help();
            std::process::exit(2);
        }
    }
}
