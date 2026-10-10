//! TLS listener integration tests (parity with onvif-go's `server`
//! TLSCertFile/TLSKeyFile support). Compiled only with the `tls` cargo
//! feature: `cargo test --features tls`.
//!
//! Certificates are generated per-run with `rcgen` (throwaway, never
//! committed — same posture as onvif-go's tls_test.go).

#![cfg(feature = "tls")]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, RootCertStore};

use onvif_device_rs::server::{OnvifActionHandler, OnvifConfig, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};

struct OkHandler;
#[async_trait]
impl OnvifActionHandler for OkHandler {
    async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        Ok("<GetProfilesResponse>tls</GetProfilesResponse>".to_string())
    }
}

/// Throwaway self-signed certificate/key PEMs in a temp dir.
fn self_signed_cert(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate self-signed cert");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, certified.cert.pem()).expect("write cert pem");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("write key pem");
    (cert_path, key_path)
}

fn tls_config(cert: &str, key: &str) -> OnvifConfig {
    OnvifConfig {
        port: 0,
        username: "admin".to_string(),
        password: "pass".to_string(),
        tls_cert_file: cert.to_string(),
        tls_key_file: key.to_string(),
        ..Default::default()
    }
}

/// A rustls client verifier that accepts the freshly generated test
/// certificate (test-only stand-in for a real trust store).
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<tokio_rustls::rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn tls_client(
    port: u16,
) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let name = ServerName::try_from("localhost".to_string())?;
    Ok(connector.connect(name, tcp).await?)
}

const SOAP_BODY: &str = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>admin</Username><Password>pass</Password></UsernameToken></Security></Header><Body><GetProfiles/></Body></Envelope>";

/// End-to-end over TLS: an HTTPS SOAP POST reaches the registered handler
/// and its response comes back — the whole HTTP/SOAP/auth stack works
/// unchanged on top of a TLS session.
#[tokio::test]
async fn https_serves_soap_actions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (cert, key) = self_signed_cert(dir.path());

    let mut server = OnvifServer::new(&tls_config(
        cert.to_str().expect("utf8 path"),
        key.to_str().expect("utf8 path"),
    ));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    let mut tls = tls_client(port).await?;
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        SOAP_BODY.len(),
        SOAP_BODY
    );
    tls.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await?;
    let resp = String::from_utf8_lossy(&buf);
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "expected 200 over TLS, got: {}",
        resp.lines().next().unwrap_or_default()
    );
    assert!(resp.contains("tls"), "handler payload: {resp}");

    handle.shutdown().await?;
    Ok(())
}

/// A TLS-configured listener does not serve plain HTTP: a cleartext
/// request fails the handshake and gets no SOAP answer.
#[tokio::test]
async fn plain_http_not_served_on_tls_listener() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (cert, key) = self_signed_cert(dir.path());

    let mut server = OnvifServer::new(&tls_config(
        cert.to_str().expect("utf8 path"),
        key.to_str().expect("utf8 path"),
    ));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    let mut plain = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        SOAP_BODY.len(),
        SOAP_BODY
    );
    plain.write_all(req.as_bytes()).await?;

    // The server drops the connection when the bytes are not a TLS
    // ClientHello: read to EOF with a bounded wait; nothing that parses as
    // an HTTP status line may come back.
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), plain.read_to_end(&mut buf)).await;
    let resp = String::from_utf8_lossy(&buf);
    assert!(
        !resp.starts_with("HTTP/1.1 200"),
        "plain HTTP must not be served on the TLS listener: {resp}"
    );

    handle.shutdown().await?;
    Ok(())
}

/// Certificate paths that do not exist surface as a configuration error
/// at `start_on` (fail fast, no silent plain-HTTP fallback).
#[tokio::test]
async fn missing_cert_file_fails_to_start() {
    let mut server = OnvifServer::new(&tls_config(
        "/nonexistent/does-not-exist-cert.pem",
        "/nonexistent/does-not-exist-key.pem",
    ));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let err = server
        .start_on(listener)
        .await
        .expect_err("missing cert file must fail");
    assert!(
        err.to_string().contains("tls_cert_file"),
        "error should name the file: {err}"
    );
}

/// RootCertStore import keeps the rustls client API honest (compile-level
/// guard for the pinned rustls version).
#[test]
fn rustls_client_types_available() {
    let _roots = RootCertStore::empty();
}
