//! End-to-end tests against a local HTTPS server (hyper + tokio-rustls) with a
//! self-signed certificate trusted through `Config::extra_roots`.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fetch::github::{self, Channel};
use fetch::{Config, Error, Expected, Fetcher, RetryPolicy};
use futures_util::stream;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ring::digest::{digest, SHA256};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const FIXTURE: &[u8] = include_bytes!("data/releases.json");

type Body = BoxBody<Bytes, std::io::Error>;

#[derive(Debug, Clone)]
struct Logged {
    path: String,
    range: Option<String>,
    accept: Option<String>,
}

struct State {
    data: Bytes,
    base: String,
    log: Mutex<Vec<Logged>>,
}

struct Server {
    base: String,
    cert: CertificateDer<'static>,
    state: Arc<State>,
}

impl Server {
    async fn start(data: Bytes) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let cert = certified.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("https://{}", listener.local_addr().unwrap());
        let state = Arc::new(State {
            data,
            base: base.clone(),
            log: Mutex::new(Vec::new()),
        });
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let st = st.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let svc = service_fn(move |req| {
                        let st = st.clone();
                        async move { Ok::<_, Infallible>(handle(&st, req)) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), svc)
                        .await;
                });
            }
        });
        Self { base, cert, state }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn config(&self) -> Config {
        Config {
            user_agent: "fetch-test".into(),
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
            rate_limit_bps: None,
            extra_roots: vec![self.cert.clone()],
            ..Config::default()
        }
    }

    fn fetcher(&self) -> Fetcher {
        Fetcher::new(self.config()).unwrap()
    }

    fn log(&self) -> Vec<Logged> {
        self.state.log.lock().unwrap().clone()
    }
}

fn full(status: StatusCode, body: Bytes) -> Response<Body> {
    let mut resp = Response::new(Full::new(body).map_err(|e| match e {}).boxed());
    *resp.status_mut() = status;
    resp
}

fn redirect(to: String) -> Response<Body> {
    let mut resp = full(StatusCode::FOUND, Bytes::new());
    resp.headers_mut().insert("location", to.parse().unwrap());
    resp
}

/// Serves `data`, honouring `Range: bytes=N-` when `honour` is set.
fn serve(data: &Bytes, range: Option<&str>, honour: bool) -> Response<Body> {
    let len = data.len();
    let start = range
        .filter(|_| honour)
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|n| n.parse::<usize>().ok());
    match start {
        None => full(StatusCode::OK, data.clone()),
        Some(n) if n >= len => {
            let mut resp = full(StatusCode::RANGE_NOT_SATISFIABLE, Bytes::new());
            resp.headers_mut()
                .insert("content-range", format!("bytes */{len}").parse().unwrap());
            resp
        }
        Some(n) => {
            let mut resp = full(StatusCode::PARTIAL_CONTENT, data.slice(n..));
            resp.headers_mut().insert(
                "content-range",
                format!("bytes {n}-{}/{len}", len - 1).parse().unwrap(),
            );
            resp
        }
    }
}

fn handle(st: &State, req: Request<hyper::body::Incoming>) -> Response<Body> {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let range = header("range");
    st.log.lock().unwrap().push(Logged {
        path: path_q.clone(),
        range: range.clone(),
        accept: header("accept"),
    });
    let path = req.uri().path();

    if let Some(n) = path.strip_prefix("/redirect/") {
        let n: u32 = n.parse().unwrap();
        return match n {
            0 => redirect("/file".into()),
            // Alternate absolute and relative Location values.
            n if n % 2 == 0 => redirect(format!("{}/redirect/{}", st.base, n - 1)),
            n => redirect(format!("{}", n - 1)),
        };
    }
    match path {
        "/file" => serve(&st.data, range.as_deref(), true),
        "/norange" => serve(&st.data, range.as_deref(), false),
        "/cut" if range.is_none() => {
            // Promise the whole file, send half, then drop the connection.
            let half = st.data.slice(..st.data.len() / 2);
            let body = stream::unfold(0u8, move |step| {
                let half = half.clone();
                async move {
                    match step {
                        0 => Some((Ok(Frame::data(half)), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(std::io::Error::other("cut")), 2))
                        }
                        _ => None,
                    }
                }
            });
            let mut resp = Response::new(StreamBody::new(body).boxed());
            resp.headers_mut()
                .insert("content-length", st.data.len().into());
            resp
        }
        "/cut" => serve(&st.data, range.as_deref(), true),
        "/badrange" if range.is_some() => {
            let mut resp = full(StatusCode::PARTIAL_CONTENT, st.data.clone());
            let len = st.data.len();
            resp.headers_mut().insert(
                "content-range",
                format!("bytes 0-{}/{len}", len - 1).parse().unwrap(),
            );
            resp
        }
        "/chunked" => {
            let chunks: Vec<Result<Frame<Bytes>, std::io::Error>> = st
                .data
                .chunks(16 * 1024)
                .map(|c| Ok(Frame::data(Bytes::copy_from_slice(c))))
                .collect();
            Response::new(StreamBody::new(stream::iter(chunks)).boxed())
        }
        "/repos/o/r/releases" => {
            let mut resp = full(StatusCode::OK, Bytes::from_static(FIXTURE));
            resp.headers_mut()
                .insert("x-ratelimit-remaining", 59.into());
            resp.headers_mut()
                .insert("x-ratelimit-reset", 1_760_000_000u64.into());
            resp
        }
        "/repos/limited/r/releases" => {
            let mut resp = full(
                StatusCode::FORBIDDEN,
                Bytes::from_static(br#"{"message":"API rate limit exceeded"}"#),
            );
            resp.headers_mut().insert("x-ratelimit-remaining", 0.into());
            resp.headers_mut()
                .insert("x-ratelimit-reset", 1_760_000_000u64.into());
            resp
        }
        _ => full(StatusCode::NOT_FOUND, Bytes::from_static(b"not found")),
    }
}

/// Deterministic pseudo-random payload.
fn payload(len: usize) -> Bytes {
    let mut x: u32 = 0x1234_5678;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect::<Vec<u8>>()
        .into()
}

fn sha(data: &[u8]) -> [u8; 32] {
    digest(&SHA256, data).as_ref().try_into().unwrap()
}

fn expected(data: &[u8]) -> Expected {
    Expected {
        size: data.len() as u64,
        sha256: sha(data),
    }
}

/// Hand-rolled temp dir (the workspace has no tempfile crate).
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fetch-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn part(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

const LEN: usize = 300 * 1024;

#[tokio::test(flavor = "current_thread")]
async fn plain_download_with_sha() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("plain");
    let dest = dir.path("a.kup");
    let mut reports = 0;
    let done = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, Some(expected(&data)), |p| {
            reports += 1;
            assert!(p.downloaded <= LEN as u64);
            assert_eq!(p.total, Some(LEN as u64));
        })
        .await
        .unwrap();
    assert_eq!(done.size, LEN as u64);
    assert_eq!(done.sha256, sha(&data));
    assert_eq!(done.resumed_from, 0);
    assert!(reports >= 2);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!part(&dest).exists());
    // Without `expected` the digest is still reported.
    let other = dir.path("b.kup");
    let done = srv
        .fetcher()
        .download_to(&srv.url("/file"), &other, None, |_| {})
        .await
        .unwrap();
    assert_eq!(done.sha256, sha(&data));
}

#[tokio::test(flavor = "current_thread")]
async fn resume_after_connection_cut() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("cut");
    let dest = dir.path("a.kup");
    let fetcher = srv.fetcher();

    // A single attempt fails with a transient error and keeps the prefix.
    let err = fetcher
        .download_to(&srv.url("/cut"), &dest, Some(expected(&data)), |_| {})
        .await
        .unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    let kept = std::fs::metadata(part(&dest)).unwrap().len();
    assert!(kept > 0 && kept < LEN as u64, "kept {kept}");

    // The retrying form resumes with Range and finishes.
    let policy = RetryPolicy {
        attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
    };
    let done = fetcher
        .download_with_retry(
            &srv.url("/cut"),
            &dest,
            Some(expected(&data)),
            policy,
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(done.resumed_from, kept);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let log = srv.log();
    assert_eq!(log.last().unwrap().range, Some(format!("bytes={kept}-")));
}

#[tokio::test(flavor = "current_thread")]
async fn retry_from_scratch_after_cut() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("retry");
    let dest = dir.path("a.kup");
    let policy = RetryPolicy {
        attempts: 2,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
    };
    let done = srv
        .fetcher()
        .download_with_retry(
            &srv.url("/cut"),
            &dest,
            Some(expected(&data)),
            policy,
            |_| {},
        )
        .await
        .unwrap();
    assert!(done.resumed_from > 0);
    assert_eq!(srv.log().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn resume_existing_prefix() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("prefix");
    let dest = dir.path("a.kup");
    std::fs::write(part(&dest), &data[..100_000]).unwrap();
    let done = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, Some(expected(&data)), |_| {})
        .await
        .unwrap();
    assert_eq!(done.resumed_from, 100_000);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(srv.log()[0].range.as_deref(), Some("bytes=100000-"));
}

#[tokio::test(flavor = "current_thread")]
async fn server_ignoring_range_restarts() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("norange");
    let dest = dir.path("a.kup");
    // A wrong prefix: if it were kept, the hash would not match.
    std::fs::write(part(&dest), vec![0xAA; 1000]).unwrap();
    let done = srv
        .fetcher()
        .download_to(&srv.url("/norange"), &dest, Some(expected(&data)), |_| {})
        .await
        .unwrap();
    assert_eq!(done.sha256, sha(&data));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(srv.log()[0].range.as_deref(), Some("bytes=1000-"));
}

#[tokio::test(flavor = "current_thread")]
async fn complete_part_skips_network() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("skip");
    let dest = dir.path("a.kup");
    std::fs::write(part(&dest), &data).unwrap();
    let done = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, Some(expected(&data)), |_| {})
        .await
        .unwrap();
    assert_eq!(done.resumed_from, LEN as u64);
    assert!(srv.log().is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test(flavor = "current_thread")]
async fn complete_part_without_expected_uses_416() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("416");
    let dest = dir.path("a.kup");
    std::fs::write(part(&dest), &data).unwrap();
    let done = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, None, |_| {})
        .await
        .unwrap();
    assert_eq!(done.sha256, sha(&data));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_content_range_is_rejected() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("badrange");
    let dest = dir.path("a.kup");
    std::fs::write(part(&dest), &data[..1000]).unwrap();
    let err = srv
        .fetcher()
        .download_to(&srv.url("/badrange"), &dest, None, |_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::BadContentRange { expected: 1000, .. }),
        "{err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn redirect_chain_is_followed() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("redirect");
    let dest = dir.path("a.kup");
    // /redirect/4 → 3 → 2 → 1 → 0 → /file: five hops, the default limit.
    srv.fetcher()
        .download_to(
            &srv.url("/redirect/4"),
            &dest,
            Some(expected(&data)),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(srv.log().len(), 6);
    assert_eq!(srv.log().last().unwrap().path, "/file");
}

#[tokio::test(flavor = "current_thread")]
async fn too_many_redirects() {
    let srv = Server::start(payload(10)).await;
    let err = srv
        .fetcher()
        .get_bytes(&srv.url("/redirect/5"), 1024)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Redirects { max: 5, .. }), "{err:?}");
    assert!(!err.is_transient());
}

#[tokio::test(flavor = "current_thread")]
async fn hash_mismatch_keeps_no_file() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("hash");
    let dest = dir.path("a.kup");
    let mut bad = expected(&data);
    bad.sha256[0] ^= 1;
    let err = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, Some(bad), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, Error::HashMismatch { .. }), "{err:?}");
    assert!(!err.is_transient());
    assert!(!dest.exists());
    assert!(!part(&dest).exists());
}

#[tokio::test(flavor = "current_thread")]
async fn size_mismatch_detected_from_headers() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("size");
    let dest = dir.path("a.kup");
    let mut bad = expected(&data);
    bad.size -= 1;
    let err = srv
        .fetcher()
        .download_to(&srv.url("/file"), &dest, Some(bad), |_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::SizeMismatch { actual, .. } if actual == LEN as u64),
        "{err:?}"
    );
    assert!(!dest.exists() && !part(&dest).exists());
}

#[tokio::test(flavor = "current_thread")]
async fn get_bytes_size_cap() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let fetcher = srv.fetcher();
    // Declared Content-Length over the cap.
    let err = fetcher
        .get_bytes(&srv.url("/file"), 1000)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge { max: 1000, .. }), "{err:?}");
    // Chunked body, cap enforced while streaming.
    let err = fetcher
        .get_bytes(&srv.url("/chunked"), 20_000)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
    let body = fetcher
        .get_bytes(&srv.url("/chunked"), LEN as u64)
        .await
        .unwrap();
    assert_eq!(body, data);
    let err = fetcher
        .get_bytes(&srv.url("/missing"), 1000)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Http { status: 404, .. }), "{err:?}");
    assert!(!err.is_transient());
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_is_roughly_respected() {
    let data = payload(LEN);
    let srv = Server::start(data.clone()).await;
    let dir = TempDir::new("rate");
    let dest = dir.path("a.kup");
    let fetcher = Fetcher::new(Config {
        rate_limit_bps: Some(128 * 1024),
        ..srv.config()
    })
    .unwrap();
    let start = Instant::now();
    fetcher
        .download_to(&srv.url("/file"), &dest, Some(expected(&data)), |_| {})
        .await
        .unwrap();
    let elapsed = start.elapsed();
    // 300 KiB at 128 KiB/s with a 32 KiB burst ≈ 2.1 s; be lenient.
    assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn untrusted_certificate_is_tls_error() {
    let srv = Server::start(payload(10)).await;
    let fetcher = Fetcher::new(Config {
        extra_roots: Vec::new(),
        ..srv.config()
    })
    .unwrap();
    let err = fetcher.get_bytes(&srv.url("/file"), 100).await.unwrap_err();
    assert!(matches!(err, Error::Tls { .. }), "{err:?}");
    assert!(!err.is_transient());
}

#[tokio::test(flavor = "current_thread")]
async fn refused_connection_is_transient() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let fetcher = Fetcher::new(Config::default()).unwrap();
    let err = fetcher
        .get_bytes(&format!("https://{addr}/x"), 100)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Connect { .. }), "{err:?}");
    assert!(err.is_transient());
}

#[tokio::test(flavor = "current_thread")]
async fn github_release_list() {
    let srv = Server::start(payload(10)).await;
    let list = github::list_releases_at(&srv.fetcher(), &srv.base, "o/r", 10)
        .await
        .unwrap();
    assert_eq!(list.releases.len(), 4);
    assert_eq!(list.rate_limit.remaining, Some(59));
    assert_eq!(
        github::newest(&list.releases, Channel::Beta)
            .unwrap()
            .tag_name,
        "v0.4.0-rc1"
    );
    let req = &srv.log()[0];
    assert_eq!(req.path, "/repos/o/r/releases?per_page=10");
    assert_eq!(req.accept.as_deref(), Some("application/vnd.github+json"));
}

#[tokio::test(flavor = "current_thread")]
async fn github_rate_limit_error() {
    let srv = Server::start(payload(10)).await;
    let err = github::list_releases_at(&srv.fetcher(), &srv.base, "limited/r", 10)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::RateLimited {
                reset: Some(1_760_000_000)
            }
        ),
        "{err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn get_json_with_headers() {
    let srv = Server::start(payload(10)).await;
    let headers = fetch::headers([("accept", "application/json")]).unwrap();
    let releases: Vec<github::Release> = srv
        .fetcher()
        .get_json_with(&srv.url("/repos/o/r/releases"), &headers, 1 << 20)
        .await
        .unwrap();
    assert_eq!(releases.len(), 4);
    assert_eq!(srv.log()[0].accept.as_deref(), Some("application/json"));
}
