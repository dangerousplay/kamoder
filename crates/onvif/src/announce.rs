//! WS-Discovery: the library's responder, restarted when the address moves.
//!
//! `onvif_device_rs::DiscoveryServer` answers Probes (multicast, and unicast —
//! the reply goes to the datagram's source, so a Probe forwarded through NAT
//! works), sends Hello on start and Bye on stop. What it cannot do is notice
//! the camera's address changing: the XAddrs and the multicast membership are
//! fixed when it starts. [`Announcer::refresh`] re-detects the address and,
//! on a change, stops the old responder (Bye from the old address) and starts
//! a new one (Hello from the new), keeping the same endpoint UUID so NVRs see
//! the same device.

use crate::net::AddressSource;
use onvif_device_rs::discovery::{DiscoveryHandle, DiscoveryServer};
use std::net::Ipv4Addr;

/// What goes into every ProbeMatch/Hello besides the address.
#[derive(Debug, Clone)]
pub struct Advert {
    pub onvif_port: u16,
    pub name: String,
    pub hardware: String,
    /// Stable `uuid:…` endpoint reference; `None` lets the library pick a
    /// random one per start.
    pub uuid: Option<String>,
    /// Extra scope URIs (`--scopes`).
    pub extra_scopes: Vec<String>,
}

impl Advert {
    /// The responder for `ip`.
    pub fn responder(&self, ip: Ipv4Addr) -> DiscoveryServer {
        let base = DiscoveryServer::with_identity(
            &ip.to_string(),
            self.onvif_port,
            &self.name,
            &self.hardware,
        );
        let mut scopes = base.scopes().to_vec();
        scopes.push("onvif://www.onvif.org/type/video_encoder".into());
        scopes.push("onvif://www.onvif.org/type/NetworkVideoTransmitter".into());
        scopes.extend(self.extra_scopes.iter().cloned());
        let server = base
            .with_scopes(scopes)
            .with_metrics(std::sync::Arc::new(crate::service::Metrics));
        match &self.uuid {
            Some(uuid) => server.with_uuid(uuid),
            None => server,
        }
    }
}

/// A stable endpoint UUID derived from the MAC address, so the camera keeps
/// its identity in the NVR across restarts without a state file.
pub fn uuid_from_mac(mac: &str) -> String {
    // Version-4 layout (the "4" and "8" nibbles) with the MAC as the node
    // field; the fixed prefix spells "onvif" loosely in hex.
    format!("uuid:616e796b-610d-4f6e-8f69-{mac}")
}

pub struct Announcer {
    source: AddressSource,
    advert: Advert,
    running: Option<(Ipv4Addr, DiscoveryHandle)>,
}

impl Announcer {
    pub fn new(source: AddressSource, advert: Advert) -> Self {
        Self {
            source,
            advert,
            running: None,
        }
    }

    /// The address currently advertised.
    pub fn address(&self) -> Option<Ipv4Addr> {
        self.running.as_ref().map(|(ip, _)| *ip)
    }

    /// Re-detects the address and (re)starts the responder if it changed.
    /// Failures are logged and retried on the next call.
    pub async fn refresh(&mut self) {
        let detected = self.source.detect();
        if detected == self.address() && (detected.is_none() || self.running.is_some()) {
            return;
        }
        if let Some((old, _)) = &self.running {
            tracing::info!(%old, new = ?detected, "address changed; re-announcing");
        }
        self.stop().await;
        let Some(ip) = detected else {
            tracing::warn!("no IPv4 address yet; WS-Discovery waits for one");
            return;
        };
        match self.advert.responder(ip).start().await {
            Ok(handle) => {
                tracing::info!(%ip, port = self.advert.onvif_port, "WS-Discovery responder up (Hello sent)");
                self.running = Some((ip, handle));
            }
            Err(error) => tracing::warn!(%ip, %error, "cannot start WS-Discovery; retrying later"),
        }
    }

    /// Stops the responder, which multicasts Bye.
    pub async fn stop(&mut self) {
        if let Some((ip, mut handle)) = self.running.take() {
            if let Err(error) = handle.shutdown().await {
                tracing::warn!(%ip, %error, "WS-Discovery responder did not stop cleanly");
            } else {
                tracing::info!(%ip, "WS-Discovery responder stopped (Bye sent)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert() -> Advert {
        Advert {
            onvif_port: 8000,
            name: "Front door".into(),
            hardware: "AK3918AV100".into(),
            uuid: Some(uuid_from_mac("001a2b3c4d5e")),
            extra_scopes: vec!["onvif://www.onvif.org/location/garage".into()],
        }
    }

    const PROBE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2004/09/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<s:Header><a:MessageID>uuid:0a6dc791-2be6-4991-9af1-454778a1917a</a:MessageID><a:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To><a:Action>http://schemas.xmlsoap.org/ws/2004/09/discovery/Probe</a:Action></s:Header>
<s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>"#;

    #[test]
    fn probe_match_carries_xaddrs_scopes_and_stable_uuid() {
        let responder = advert().responder(Ipv4Addr::new(192, 0, 2, 10));
        let reply =
            String::from_utf8(responder.handle_probe(PROBE.as_bytes(), "").unwrap()).unwrap();
        assert!(reply.contains("ProbeMatch"), "{reply}");
        assert!(
            reply.contains("<d:XAddrs>http://192.0.2.10:8000/onvif/device_service</d:XAddrs>"),
            "{reply}"
        );
        assert!(reply.contains("NetworkVideoTransmitter"), "{reply}");
        assert!(
            reply.contains("uuid:0a6dc791-2be6-4991-9af1-454778a1917a"),
            "{reply}"
        );
        assert!(
            reply.contains("uuid:616e796b-610d-4f6e-8f69-001a2b3c4d5e"),
            "{reply}"
        );
        assert!(
            reply.contains("onvif://www.onvif.org/name/Front%20door"),
            "{reply}"
        );
        assert!(
            reply.contains("onvif://www.onvif.org/location/garage"),
            "{reply}"
        );

        let bye = String::from_utf8(responder.build_bye()).unwrap();
        assert!(bye.contains("uuid:616e796b-610d-4f6e-8f69-001a2b3c4d5e"));
    }

    /// A real unicast Probe over UDP. The library binds the fixed WS-Discovery
    /// port 3702 (it has no port parameter), so this is skipped when the host
    /// cannot bind it or join multicast on loopback — the ProbeMatch content
    /// itself is covered by the test above.
    #[tokio::test(flavor = "current_thread")]
    async fn unicast_probe_over_udp() {
        let mut announcer = Announcer::new(AddressSource::Fixed(Ipv4Addr::LOCALHOST), advert());
        announcer.refresh().await;
        if announcer.address().is_none() {
            eprintln!("skipping: cannot start a WS-Discovery responder on udp/3702 here");
            return;
        }
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(PROBE.as_bytes(), "127.0.0.1:3702")
            .await
            .unwrap();
        let mut buf = vec![0u8; 8192];
        let reply = loop {
            let (n, _) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client.recv_from(&mut buf),
            )
            .await
            .expect("no ProbeMatch within 3 s")
            .unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            // Another responder on the host may answer too; wait for ours.
            if text.contains("616e796b-610d") {
                break text;
            }
        };
        assert!(reply.contains("ProbeMatch"), "{reply}");
        assert!(
            reply.contains("http://127.0.0.1:8000/onvif/device_service"),
            "{reply}"
        );
        announcer.stop().await;
        assert_eq!(announcer.address(), None);
    }
}
