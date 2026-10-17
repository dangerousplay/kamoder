//! HTTPS beside HTTP, with a self-signed certificate (AUD-18).
//!
//! Why TLS at all on a LAN tool: browsers only hand `getUserMedia` (the
//! microphone, for push-to-talk) to a *secure context*, and a camera on a
//! home LAN has no name a CA would sign. So `web` generates its own
//! certificate on first start and the user accepts it once; plain HTTP stays
//! up for everything else (`context.md`: "Self-signed HTTPS in `web`").
//!
//! ```text
//! web --listen 0.0.0.0:80 --tls-listen 0.0.0.0:443 \
//!     --tls-cert etc/web-tls.crt --tls-key etc/web-tls.key
//! ```
//!
//! - **Certificate**: ECDSA P-256 (cheap to sign on an ARM926; RSA-2048 keygen
//!   alone takes seconds there), self-signed, valid [`VALIDITY_DAYS`]. The
//!   SAN lists the hostname, `localhost` and every non-loopback IPv4 address
//!   up at generation time. Missing or unparseable files are regenerated; a
//!   valid pair is reused even if the addresses changed since, because a new
//!   certificate means a new browser warning. Delete the files to force one.
//! - **Clock**: the camera has no RTC and boots in 1970 until NTP runs, so
//!   `notBefore` is never earlier than [`EPOCH_FLOOR`]; a certificate made
//!   before NTP is still valid in browsers for ten years from that date.
//! - **Crypto**: rustls/rcgen on `ring` only (aws-lc does not build for
//!   ARMv5TE), TLS 1.2 and 1.3, ALPN `http/1.1` (the server has no HTTP/2).
//! - **Listener**: [`TlsListener`] is an [`axum::serve::Listener`], so the
//!   HTTPS server shares the router and the graceful shutdown of the HTTP
//!   one. Handshakes run as their own tasks, at most [`MAX_HANDSHAKES`] at
//!   once and each within [`HANDSHAKE_TIMEOUT`], so a client that connects
//!   and says nothing cannot hold the accept loop or a slot for long.
//!
//! Any failure here is logged by the caller and the server carries on with
//! HTTP only.

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use snafu::{ResultExt, Snafu};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// Certificate lifetime: ten years, so a camera never needs a renewal path.
pub const VALIDITY_DAYS: i64 = 3650;
/// Earliest `notBefore`, Unix seconds (2026-01-01): a certificate generated
/// while the clock still says 1970 must not be expired the moment NTP runs.
pub const EPOCH_FLOOR: i64 = 1_767_225_600;
/// A client must finish the TLS handshake within this.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshakes in progress at once; further connections wait in the kernel's
/// accept queue. Each holds a socket and ~40 KB of rustls state.
pub const MAX_HANDSHAKES: usize = 8;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: rustls::pki_types::pem::Error,
    },

    #[snafu(display("{} holds no certificate", path.display()))]
    NoCertificate { path: PathBuf },

    #[snafu(display("rustls rejects the certificate or key: {source}"))]
    Config { source: rustls::Error },

    #[snafu(display("cannot generate a certificate: {source}"))]
    Generate { source: rcgen::Error },

    #[snafu(display("cannot write {}: {source}", path.display()))]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// What a generated certificate is issued for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectNames {
    /// DNS names; the first is also the subject's common name.
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
}

impl SubjectNames {
    /// This machine: its hostname, `localhost`, and its non-loopback IPv4
    /// addresses.
    pub fn local() -> Self {
        let mut dns = Vec::new();
        if let Some(host) = hostname() {
            dns.push(host);
        }
        if !dns.iter().any(|name| name == "localhost") {
            dns.push("localhost".to_owned());
        }
        let ips = ipv4_addresses().into_iter().map(IpAddr::V4).collect();
        Self { dns, ips }
    }
}

/// The server configuration from `cert` and `key`, generating both first if
/// they are missing or unusable.
pub fn load_or_generate(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, Error> {
    match load(cert, key) {
        Ok(config) => return Ok(config),
        Err(error) if cert.exists() || key.exists() => {
            tracing::warn!(%error, "unusable TLS certificate; generating a new one");
        }
        Err(_) => tracing::info!(cert = %cert.display(), "no TLS certificate; generating one"),
    }
    let names = SubjectNames::local();
    generate(cert, key, &names, SystemTime::now())?;
    tracing::info!(dns = ?names.dns, ips = ?names.ips, "generated a self-signed TLS certificate");
    load(cert, key)
}

/// The server configuration from existing PEM files.
pub fn load(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, Error> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .context(ReadSnafu { path: cert })?;
    snafu::ensure!(!chain.is_empty(), NoCertificateSnafu { path: cert });
    let key = PrivateKeyDer::from_pem_file(key).context(ReadSnafu { path: key })?;
    server_config(chain, key)
}

fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, Error> {
    // Explicit provider: no process-wide default is installed, and rustls
    // must never go looking for aws-lc.
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context(ConfigSnafu)?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context(ConfigSnafu)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Generates a self-signed ECDSA P-256 certificate for `names`, valid from
/// `now` (or [`EPOCH_FLOOR`], whichever is later) for [`VALIDITY_DAYS`], and
/// writes key and certificate atomically with mode 0600.
pub fn generate(
    cert: &Path,
    key: &Path,
    names: &SubjectNames,
    now: SystemTime,
) -> Result<(), Error> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).context(GenerateSnafu)?;

    let mut params = CertificateParams::default();
    let mut subject = DistinguishedName::new();
    subject.push(
        DnType::CommonName,
        names.dns.first().map_or("kamoder", String::as_str),
    );
    subject.push(DnType::OrganizationName, "Kamoder");
    params.distinguished_name = subject;
    params.subject_alt_names = names
        .dns
        .iter()
        .filter_map(|name| match name.clone().try_into() {
            Ok(name) => Some(SanType::DnsName(name)),
            Err(error) => {
                tracing::warn!(%error, name, "left out of the certificate");
                None
            }
        })
        .chain(names.ips.iter().copied().map(SanType::IpAddress))
        .collect();
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let since = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        // An hour of slack for a browser clock slightly behind the camera's.
        .saturating_sub(3600)
        .max(EPOCH_FLOOR);
    params.not_before = time::OffsetDateTime::from_unix_timestamp(since)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    params.not_after = params.not_before + time::Duration::days(VALIDITY_DAYS);

    let certificate = params.self_signed(&key_pair).context(GenerateSnafu)?;
    // Key first: a crash in between leaves a key without a certificate,
    // which the next start treats as missing and regenerates.
    write_private(key, key_pair.serialize_pem().as_bytes())?;
    write_private(cert, certificate.pem().as_bytes())
}

fn write_private(path: &Path, contents: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).context(WriteSnafu { path })?;
    }
    ::auth::atomic::write(path, contents, 0o600).context(WriteSnafu { path })
}

/// The kernel's hostname, unless empty or the uninformative `(none)`.
fn hostname() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = name.trim();
    (!name.is_empty() && name != "(none)").then(|| name.to_owned())
}

/// Every non-loopback IPv4 address on an interface, without duplicates.
fn ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list it allocated, or fails
    // and leaves it alone; uClibc 0.9.33 exports it (checked with nm).
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "getifaddrs failed");
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut node = head;
    while !node.is_null() {
        // SAFETY: `node` is an element of the list `getifaddrs` returned,
        // which stays valid until `freeifaddrs` below.
        let entry = unsafe { &*node };
        let addr = entry.ifa_addr;
        // SAFETY: a non-null `ifa_addr` points at a sockaddr whose family
        // field is always readable; for AF_INET it is a `sockaddr_in`.
        if !addr.is_null() && i32::from(unsafe { (*addr).sa_family }) == libc::AF_INET {
            // SAFETY: as above, the family says this is a `sockaddr_in`.
            let sin = unsafe { &*(addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !ip.is_loopback() && !ip.is_unspecified() && !found.contains(&ip) {
                found.push(ip);
            }
        }
        node = entry.ifa_next;
    }
    // SAFETY: `head` came from a successful `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(head) };
    found
}

/// A TCP listener that hands out TLS streams once their handshake is done.
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
    timeout: Duration,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> Self {
        Self {
            tcp,
            acceptor: TlsAcceptor::from(config),
            handshakes: JoinSet::new(),
            timeout: HANDSHAKE_TIMEOUT,
        }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                accepted = self.tcp.accept(), if self.handshakes.len() < MAX_HANDSHAKES => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let acceptor = self.acceptor.clone();
                            let timeout = self.timeout;
                            self.handshakes.spawn(handshake(acceptor, stream, peer, timeout));
                        }
                        Err(error) => accept_failed(error).await,
                    }
                }
                Some(done) = self.handshakes.join_next() => {
                    if let Ok(Some(connection)) = done {
                        return connection;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    peer: SocketAddr,
    timeout: Duration,
) -> Option<(TlsStream<TcpStream>, SocketAddr)> {
    match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => Some((tls, peer)),
        // Expected whenever a browser has not accepted the certificate yet.
        Ok(Err(error)) => {
            tracing::debug!(%peer, %error, "TLS handshake failed");
            None
        }
        Err(_) => {
            tracing::debug!(%peer, "TLS handshake timed out");
            None
        }
    }
}

/// Like axum's own TCP listener: per-connection errors are skipped, anything
/// else (out of file descriptors) backs off a second instead of spinning.
async fn accept_failed(error: std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if matches!(
        error.kind(),
        ConnectionAborted | ConnectionRefused | ConnectionReset
    ) {
        return;
    }
    tracing::error!(%error, "TLS accept failed");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::scratch;

    fn names() -> SubjectNames {
        SubjectNames {
            dns: vec!["camera".into(), "localhost".into()],
            ips: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        }
    }

    #[test]
    fn generated_pair_is_private_and_reloads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("tls-generate");
        let (cert, key) = (dir.join("etc/web-tls.crt"), dir.join("etc/web-tls.key"));
        generate(&cert, &key, &names(), SystemTime::now()).unwrap();
        for path in [&cert, &key] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
        assert!(std::fs::read_to_string(&cert)
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"));
        load(&cert, &key).unwrap();
    }

    #[test]
    fn missing_or_corrupt_files_are_regenerated_and_good_ones_kept() {
        let dir = scratch("tls-regenerate");
        let (cert, key) = (dir.join("web-tls.crt"), dir.join("web-tls.key"));
        load_or_generate(&cert, &key).unwrap();
        let first = std::fs::read(&cert).unwrap();
        load_or_generate(&cert, &key).unwrap();
        assert_eq!(
            std::fs::read(&cert).unwrap(),
            first,
            "a valid pair is reused"
        );

        std::fs::write(&cert, "garbage").unwrap();
        load_or_generate(&cert, &key).unwrap();
        assert_ne!(std::fs::read(&cert).unwrap(), first);
    }

    #[test]
    fn local_names_include_localhost() {
        let names = SubjectNames::local();
        assert!(names.dns.iter().any(|n| n == "localhost"));
        assert!(names.ips.iter().all(|ip| !ip.is_loopback()));
    }

    /// A rustls client that trusts only the generated certificate fetches a
    /// page over [`TlsListener`]: chain, SAN `localhost`, ALPN and the
    /// listener all work together.
    #[tokio::test(flavor = "current_thread")]
    async fn https_round_trip_with_the_generated_certificate() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = scratch("tls-serve");
        let (cert, key) = (dir.join("web-tls.crt"), dir.join("web-tls.key"));
        // 1970 clock: the floor must still give a certificate valid today.
        generate(&cert, &key, &names(), UNIX_EPOCH).unwrap();
        let config = load(&cert, &key).unwrap();

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let app = axum::Router::new().route("/hello", axum::routing::get(|| async { "hi" }));
        tokio::spawn(async move {
            axum::serve(TlsListener::new(tcp, config), app)
                .await
                .unwrap();
        });

        // A silent client must not block the next one.
        let _silent = TcpStream::connect(addr).await.unwrap();

        let mut roots = rustls::RootCertStore::empty();
        for der in CertificateDer::pem_file_iter(&cert).unwrap() {
            roots.add(der.unwrap()).unwrap();
        }
        let mut client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(name, tcp).await.unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        tls.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tls.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("hi"), "{response}");
    }
}
