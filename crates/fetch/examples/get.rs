//! Minimal downloader, mainly to measure what `fetch` adds to a binary.
//!
//! ```text
//! get <https-url> <dest> [rate-bytes-per-sec|0]
//! ```

// Pulls in the `pthread_setname_np` shim the camera's uClibc lacks.
extern crate os;

use std::path::Path;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, url, dest, rest @ ..] = args.as_slice() else {
        eprintln!("usage: get <https-url> <dest> [rate-bytes-per-sec|0]");
        std::process::exit(2);
    };
    let rate = match rest.first().map(|r| r.parse::<u32>()) {
        None => Some(fetch::DEFAULT_RATE_LIMIT_BPS),
        Some(Ok(0)) => None,
        Some(Ok(n)) => Some(n),
        Some(Err(e)) => {
            eprintln!("bad rate: {e}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime.block_on(async {
        let fetcher = fetch::Fetcher::new(fetch::Config {
            rate_limit_bps: rate,
            ..fetch::Config::default()
        })?;
        let policy = fetch::RetryPolicy {
            initial_backoff: Duration::from_secs(1),
            ..fetch::RetryPolicy::default()
        };
        fetcher
            .download_with_retry(url, Path::new(dest), None, policy, |p| {
                eprint!("\r{} / {:?} bytes", p.downloaded, p.total);
            })
            .await
    });
    match result {
        Ok(done) => eprintln!("\n{} bytes, sha256 {}", done.size, fetch::hex(&done.sha256)),
        Err(e) => {
            eprintln!("\nerror: {e}");
            std::process::exit(1);
        }
    }
}
