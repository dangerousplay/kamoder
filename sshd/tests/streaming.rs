//! End-to-end streaming tests against the real `sshd` binary.
//!
//! Regression for the camera hang where `ssh cam 'cat > file' < big.tgz`
//! wedged the whole server: stdin was written to the child with a blocking
//! `write(2)` on the runtime thread, so one slow consumer stopped every other
//! connection, down to the banner exchange.
//!
//! These run the server as a separate process on 127.0.0.1 and drive it with a
//! `russh` client, so the server's RSS can be read from `/proc/<pid>/status`
//! without the client's own buffers muddying the number.

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh::ChannelMsg;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USER: &str = "tester";
const PASSWORD: &str = "correct horse";
/// Upload size for the slow-consumer test.
const UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// How much the server may grow while streaming. The stdin path is bounded to
/// a few hundred KiB; anything near this means input is being queued.
const RSS_BUDGET_KB: usize = 8 * 1024;
/// A second client must get the server's banner within this, mid-upload.
const BANNER_DEADLINE: Duration = Duration::from_secs(2);
const CHUNK: usize = 32 * 1024;

/// The server process plus its scratch directory; both cleaned up on drop.
struct Server {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Server {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("anyka-sshd-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Few iterations: the tests are about the data path, not PBKDF2.
        let line = auth::credentials::hash_password(USER, PASSWORD, 1000).unwrap();
        let passwd = dir.join("passwd");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&passwd)
            .unwrap();
        writeln!(file, "{line}").unwrap();

        // Pick a free port. The window between closing this listener and the
        // server binding it is small enough for a test.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let log = std::fs::File::create(dir.join("server.log")).unwrap();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_sshd"))
            .args(["--listen", "127.0.0.1", "--port", &port.to_string()])
            .arg("--passwd")
            .arg(&passwd)
            .arg("--host-key")
            .arg(dir.join("host.key"))
            .env("RUST_LOG", "info")
            .env_remove("ANYKA_LOG_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();

        let server = Self { child, port, dir };
        let deadline = Instant::now() + Duration::from_secs(20);
        while banner(port, Duration::from_secs(1)).is_err() {
            assert!(Instant::now() < deadline, "server never came up");
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Reads the SSH identification line, as a client does before anything else.
fn banner(port: u16, timeout: Duration) -> std::io::Result<String> {
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.starts_with("SSH-2.0-") {
        Ok(line)
    } else {
        Err(std::io::Error::other(format!("unexpected banner {line:?}")))
    }
}

fn vm_rss_kb(pid: u32) -> Option<usize> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

struct TrustAnyHost;

impl client::Handler for TrustAnyHost {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn login(port: u16) -> client::Handle<TrustAnyHost> {
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, ("127.0.0.1", port), TrustAnyHost)
        .await
        .expect("connect");
    let auth = session
        .authenticate_password(USER, PASSWORD)
        .await
        .expect("authenticate");
    assert!(auth.success(), "password rejected");
    session
}

/// Runs `command` without input and returns (stdout, exit status).
async fn run(port: u16, command: &str) -> (Vec<u8>, Option<u32>) {
    let session = login(port).await;
    let mut channel = session.channel_open_session().await.unwrap();
    channel.exec(true, command).await.unwrap();
    channel.eof().await.unwrap();
    collect(&mut channel).await
}

async fn collect(channel: &mut russh::Channel<client::Msg>) -> (Vec<u8>, Option<u32>) {
    let mut output = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => output.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    (output, status)
}

/// Deterministic, incompressible-looking test data (xorshift64).
struct Noise(u64);

impl Noise {
    fn fill(&mut self, buffer: &mut [u8]) {
        for word in buffer.chunks_mut(8) {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            word.copy_from_slice(&self.0.to_le_bytes()[..word.len()]);
        }
    }
}

fn sha256_file(path: &Path) -> Vec<u8> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut file = std::fs::File::open(path).unwrap();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut file, &mut buffer).unwrap();
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    context.finish().as_ref().to_vec()
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A consumer slower than the network, standing in for a FAT-formatted SD
/// card: copies stdin to the file in 16 KiB steps with a pause between them.
fn slow_writer_command(target: &Path) -> String {
    let target = target.display();
    if which("python3") {
        format!(
            "python3 -c 'import sys, time\n\
             out = open(sys.argv[1], \"wb\")\n\
             while True:\n    \
                 block = sys.stdin.buffer.read1(16384)\n    \
                 if not block: break\n    \
                 out.write(block)\n    \
                 time.sleep(0.0005)\n' {target}"
        )
    } else {
        // `dd` with a tiny block size is slower than the loopback link too.
        format!("dd of={target} bs=512 2>/dev/null")
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The camera scenario: stream a large file into a slow consumer's stdin,
/// while other clients keep connecting. The server must pace the client to the
/// consumer (bounded memory), deliver every byte, close stdin on EOF, and keep
/// serving everyone else throughout.
#[test]
fn slow_stdin_consumer_is_paced_and_does_not_block_other_clients() {
    let server = Server::start("upload");
    let target = server.dir.join("upload.bin");
    let command = slow_writer_command(&target);
    let port = server.port;

    let runtime = current_thread_runtime();
    let session = runtime.block_on(login(port));
    let baseline = vm_rss_kb(server.pid()).expect("server RSS");

    // Sample the server's RSS for the whole transfer.
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(baseline));
    let sampler = {
        let (done, peak, pid) = (done.clone(), peak.clone(), server.pid());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if let Some(rss) = vm_rss_kb(pid) {
                    peak.fetch_max(rss, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let mut probes = Vec::new();
    let started = Instant::now();
    let (sent_digest, status) = runtime.block_on(async {
        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, command.as_str()).await.unwrap();

        let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
        let mut chunk = vec![0u8; CHUNK];
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        let mut sent = 0u64;
        while sent < UPLOAD_BYTES {
            noise.fill(&mut chunk);
            context.update(&chunk);
            tokio::time::timeout(Duration::from_secs(30), channel.data(&chunk[..]))
                .await
                .expect("upload stalled for 30 s")
                .expect("upload failed");
            sent += CHUNK as u64;

            // Mid-transfer, while the consumer is backed up: other clients
            // must still be served promptly.
            if sent.is_multiple_of(16 * 1024 * 1024) {
                probes.push(std::thread::spawn(move || {
                    let asked = Instant::now();
                    let banner = banner(port, BANNER_DEADLINE);
                    let banner_after = asked.elapsed();
                    let runtime = current_thread_runtime();
                    let second = runtime.block_on(async {
                        tokio::time::timeout(
                            Duration::from_secs(10),
                            run(port, "echo second session"),
                        )
                        .await
                    });
                    (banner.map(|_| banner_after), second)
                }));
            }
        }
        // EOF must reach the child as end of stdin, or it never exits.
        channel.eof().await.unwrap();
        let (_, status) = tokio::time::timeout(Duration::from_secs(60), collect(&mut channel))
            .await
            .expect("no exit status within 60 s of EOF: stdin was not closed");
        (context.finish().as_ref().to_vec(), status)
    });
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    for probe in probes {
        let (banner, second) = probe.join().unwrap();
        let banner_after = banner.unwrap_or_else(|error| {
            panic!(
                "no banner within {BANNER_DEADLINE:?} during the upload: {error}\n{}",
                server.log()
            )
        });
        assert!(banner_after <= BANNER_DEADLINE);
        let (output, status) = second.expect("second session timed out during the upload");
        assert_eq!(output, b"second session\n");
        assert_eq!(status, Some(0));
    }

    assert_eq!(status, Some(0), "upload command failed\n{}", server.log());
    assert_eq!(
        std::fs::metadata(&target).unwrap().len(),
        UPLOAD_BYTES,
        "file size"
    );
    assert_eq!(sha256_file(&target), sent_digest, "file contents differ");

    let growth = peak.load(Ordering::Relaxed).saturating_sub(baseline);
    eprintln!(
        "uploaded {} MiB in {elapsed:?}; server RSS {baseline} kB, peak +{growth} kB",
        UPLOAD_BYTES >> 20
    );
    assert!(
        growth <= RSS_BUDGET_KB,
        "server RSS grew by {growth} kB while streaming (budget {RSS_BUDGET_KB} kB)"
    );
}

/// Binary data through stdin and back out through stdout must be untouched —
/// no terminal line discipline in between — and the two directions must not
/// deadlock each other when both are busy.
#[test]
fn exec_without_pty_is_binary_clean_in_both_directions() {
    const BYTES: usize = 8 * 1024 * 1024;
    let server = Server::start("echo");
    let runtime = current_thread_runtime();
    runtime.block_on(async {
        let session = login(server.port).await;
        let channel = session.channel_open_session().await.unwrap();
        channel.exec(true, "cat").await.unwrap();
        let (mut reader, writer) = channel.split();

        let mut noise = Noise(42);
        let mut input = vec![0u8; BYTES];
        noise.fill(&mut input);
        // Every byte value, including ^C, ^D, ^S, ^Z and CR, which a PTY
        // would have interpreted.
        for (index, byte) in input.iter_mut().take(256).enumerate() {
            *byte = index as u8;
        }

        let sender = async {
            for chunk in input.chunks(CHUNK) {
                writer.data(chunk).await.unwrap();
            }
            writer.eof().await.unwrap();
        };
        let receiver = async {
            let mut output = Vec::with_capacity(BYTES);
            let mut status = None;
            while let Some(message) = reader.wait().await {
                match message {
                    ChannelMsg::Data { data } => output.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            (output, status)
        };
        let ((), (output, status)) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(sender, receiver)
        })
        .await
        .unwrap_or_else(|_| panic!("echo through `cat` stalled\n{}", server.log()));

        assert_eq!(status, Some(0));
        assert_eq!(output.len(), input.len());
        assert!(output == input, "stdout differs from stdin");
    });
}

/// Exit status and stderr still come through for a non-PTY command.
#[test]
fn exec_reports_exit_status_and_stderr() {
    let server = Server::start("status");
    let runtime = current_thread_runtime();
    runtime.block_on(async {
        let session = login(server.port).await;
        let mut channel = session.channel_open_session().await.unwrap();
        channel
            .exec(true, "echo out; echo err >&2; exit 7")
            .await
            .unwrap();
        channel.eof().await.unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut status = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        assert_eq!(stdout, b"out\n");
        assert_eq!(stderr, b"err\n");
        assert_eq!(status, Some(7));
    });
}
