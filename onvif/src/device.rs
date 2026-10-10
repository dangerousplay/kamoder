//! GetCapabilities and GetServices, which tell a client where each service
//! lives. They list only what this process serves: Device and Media always,
//! PTZ only with a motor backend (ONVIF-06, PTZ-05: "ONVIF SHALL not
//! advertise PTZ"), Imaging only while the lights owner answers (checked per
//! call, see [`crate::imaging::available`]), never Events. The rest of the Device service
//! (GetSystemDateAndTime, GetDeviceInformation, GetScopes) is the library's
//! `DeviceHandler`, whose output is already namespace-qualified.

use crate::imaging;
use crate::soap::{request, response};
use onvif_device_rs::namespaces::{DEVICE_SERVICE, IMAGING_SERVICE, MEDIA_SERVICE, PTZ_SERVICE};
use onvif_device_rs::server::OnvifActionHandler;
use onvif_device_rs::types::{resolve_server_ip, OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{tds, tt};

/// The service endpoints, as paths under `http://host:port`.
const DEVICE_PATH: &str = "/onvif/device_service";
const MEDIA_PATH: &str = "/onvif/media_service";
const PTZ_PATH: &str = "/onvif/ptz_service";
const IMAGING_PATH: &str = "/onvif/imaging_service";

/// Where the services are reached.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The SOAP server's port.
    pub port: u16,
    /// Fallback address, used when the request arrived over loopback or on
    /// an unknown address (see [`resolve_server_ip`]).
    pub device_ip: String,
    /// PTZ is served.
    pub ptz: bool,
    /// The lights owner Imaging is served through; `None` = Imaging is off.
    pub lights: Option<lights_proto::Client>,
}

impl Endpoints {
    /// `http://host:port/path`, with the address the client reached us on.
    fn x_addr(&self, info: &RequestInfo, path: &str) -> String {
        let host = resolve_server_ip(&info.server_ip, &self.device_ip);
        format!("http://{host}:{}{path}", self.port)
    }

    /// Imaging is served and its owner answers right now.
    async fn imaging(&self) -> bool {
        match &self.lights {
            Some(lights) => imaging::available(lights).await,
            None => false,
        }
    }
}

/// GetCapabilities. Every category is answered whatever the request's
/// `Category` asks for; clients pick what they need.
pub struct GetCapabilities(pub Endpoints);

#[async_trait::async_trait]
impl OnvifActionHandler for GetCapabilities {
    async fn handle(&self, body: &str, info: &RequestInfo) -> Result<String, OnvifError> {
        request::<tds::GetCapabilities>(body)?;
        let endpoints = &self.0;
        let imaging = endpoints.imaging().await;
        let capabilities = tt::CapabilitiesType {
            any_attribute: AnyAttributes::default(),
            analytics: None,
            device: Some(tt::DeviceCapabilitiesType {
                any_attribute: AnyAttributes::default(),
                x_addr: endpoints.x_addr(info, DEVICE_PATH),
                network: None,
                system: None,
                io: None,
                security: None,
                extension: None,
            }),
            events: None,
            imaging: imaging.then(|| tt::ImagingCapabilitiesType {
                any_attribute: AnyAttributes::default(),
                x_addr: endpoints.x_addr(info, IMAGING_PATH),
            }),
            media: Some(tt::MediaCapabilitiesType {
                any_attribute: AnyAttributes::default(),
                x_addr: endpoints.x_addr(info, MEDIA_PATH),
                // RTSP over TCP (interleaved) and plain RTP/UDP; no multicast.
                streaming_capabilities: tt::RealTimeStreamingCapabilitiesType {
                    any_attribute: AnyAttributes::default(),
                    rtp_multicast: Some(false),
                    rtp_tcp: Some(true),
                    rtp_rtsp_tcp: Some(true),
                    extension: None,
                },
                any: vec![],
                extension: None,
            }),
            ptz: endpoints.ptz.then(|| tt::PtzCapabilitiesType {
                any_attribute: AnyAttributes::default(),
                x_addr: endpoints.x_addr(info, PTZ_PATH),
                any: vec![],
            }),
            extension: None,
        };
        response(
            &tds::GetCapabilitiesResponse { capabilities },
            "tds:GetCapabilitiesResponse",
        )
    }
}

/// GetServices. Service capabilities are never included, even when asked
/// for: GetServiceCapabilities is not served either. The request is not
/// parsed, since its one element (the required `IncludeCapability`) would
/// change nothing, and a client that leaves it out still gets an answer.
pub struct GetServices(pub Endpoints);

#[async_trait::async_trait]
impl OnvifActionHandler for GetServices {
    async fn handle(&self, _body: &str, info: &RequestInfo) -> Result<String, OnvifError> {
        let endpoints = &self.0;
        let mut served = vec![(DEVICE_SERVICE, DEVICE_PATH), (MEDIA_SERVICE, MEDIA_PATH)];
        if endpoints.ptz {
            served.push((PTZ_SERVICE, PTZ_PATH));
        }
        if endpoints.imaging().await {
            served.push((IMAGING_SERVICE, IMAGING_PATH));
        }
        let service = served
            .into_iter()
            .map(|(namespace, path)| tds::ServiceType {
                any_attribute: AnyAttributes::default(),
                namespace: namespace.into(),
                x_addr: endpoints.x_addr(info, path),
                capabilities: None,
                // As onvif-device-rs has always reported it.
                version: tt::OnvifVersionType { major: 1, minor: 0 },
                any: vec![],
            })
            .collect();
        response(
            &tds::GetServicesResponse { service },
            "tds:GetServicesResponse",
        )
    }
}
