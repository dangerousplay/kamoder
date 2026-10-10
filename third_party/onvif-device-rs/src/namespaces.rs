// ---------------------------------------------------------------------------
// ONVIF namespace constants
// ---------------------------------------------------------------------------

/// SOAP 1.2 Envelope namespace.
pub const SOAP_ENVELOPE: &str = "http://www.w3.org/2003/05/soap-envelope";

/// WS-Addressing namespace.
pub const WS_ADDRESSING: &str = "http://www.w3.org/2005/08/addressing";

/// WS-Security Secext (Security) namespace.
pub const WS_SECURITY: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";

/// WS-Security Utility namespace.
pub const WS_UTILITY: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";

/// ONVIF Device Service WSDL namespace.
pub const DEVICE_SERVICE: &str = "http://www.onvif.org/ver10/device/wsdl";

/// ONVIF Media Service WSDL namespace.
pub const MEDIA_SERVICE: &str = "http://www.onvif.org/ver10/media/wsdl";

/// ONVIF PTZ Service WSDL namespace.
pub const PTZ_SERVICE: &str = "http://www.onvif.org/ver20/ptz/wsdl";

/// ONVIF Imaging Service WSDL namespace.
pub const IMAGING_SERVICE: &str = "http://www.onvif.org/ver20/imaging/wsdl";

/// ONVIF Events Service WSDL namespace (`tev`).
pub const EVENTS_SERVICE: &str = "http://www.onvif.org/ver10/events/wsdl";

/// WS-BaseNotification namespace (`wsnt` — Topic, NotificationMessage,
/// CurrentTime/TerminationTime, Renew/Unsubscribe).
pub const WS_NOTIFICATION: &str = "http://docs.oasis-open.org/wsn/b-2";

/// WS-Topics namespace (`wstop` — TopicSet).
pub const WS_TOPICS: &str = "http://docs.oasis-open.org/wsn/t-1";

/// ONVIF Schema namespace (data types common across services).
pub const SCHEMAS: &str = "http://www.onvif.org/ver10/schema";
