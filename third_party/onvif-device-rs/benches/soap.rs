//! SOAP parse and auth verification throughput (issue #22): the two
//! per-request CPU costs of the ONVIF device service. Run with
//! `cargo bench --bench soap`.

use criterion::{criterion_group, criterion_main, Criterion};
use onvif_device_rs::auth::verify_username_token;
use onvif_device_rs::server::parse_soap_request;
use onvif_device_rs::types::UsernameToken;

const GET_PROFILES_PLAIN: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">\
<s:Body><GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/></s:Body>\
</s:Envelope>";

const GET_DEVICE_INFORMATION_AUTHED: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">\
<s:Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
<UsernameToken><Username>admin</Username><Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">ESJQUa0mMYOQDkVtMoTSLMYtJ+U=</Password>\
<Nonce>aGVsbG8gd29ybGQgcHJvYmU=</Nonce>\
<Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">2026-09-09T00:00:00Z</Created>\
</UsernameToken></Security></s:Header>\
<s:Body><GetDeviceInformation xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/></s:Body>\
</s:Envelope>";

fn bench_soap(c: &mut Criterion) {
    c.bench_function("soap/parse_get_profiles", |b| {
        b.iter(|| parse_soap_request(GET_PROFILES_PLAIN).expect("parse"))
    });

    c.bench_function("soap/parse_authed_get_device_information", |b| {
        b.iter(|| parse_soap_request(GET_DEVICE_INFORMATION_AUTHED).expect("parse"))
    });

    let plain = UsernameToken {
        username: "admin".into(),
        password: "secret".into(),
        nonce: String::new(),
        created: String::new(),
    };
    c.bench_function("auth/verify_plaintext", |b| {
        b.iter(|| verify_username_token(&plain, "admin", "secret"))
    });

    let digest = UsernameToken {
        username: "admin".into(),
        password: "ESJQUa0mMYOQDkVtMoTSLMYtJ+U=".into(),
        nonce: "aGVsbG8gd29ybGQgcHJvYmU=".into(),
        created: "2026-09-09T00:00:00Z".into(),
    };
    c.bench_function("auth/verify_digest_mismatch", |b| {
        b.iter(|| verify_username_token(&digest, "admin", "wrong"))
    });
}

criterion_group!(benches, bench_soap);
criterion_main!(benches);
