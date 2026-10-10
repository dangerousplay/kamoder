"""onvif as an NVR sees it: WS-Discovery, then SOAP through
onvif-zeep-async (Home Assistant's ONVIF client).

QEMU user networking carries no multicast, so discovery is a unicast Probe to
the forwarded UDP port (the responder answers the datagram's source). The
camera builds every URL from the address a request arrived on — the guest's
10.0.2.15 — which the host cannot reach; the client runs with
`nat_override`, which rewrites the service XAddrs to the forwarded
host:port (all services share port 8000, so that is exact).

PTZ is the simulated head (ANYKA_ONVIF_PTZ=sim in firmware-init), the same
one web drives through /tmp/anyka-ptz.sock.
"""

import asyncio
import base64
import datetime as dt
import hashlib
import os
import re
import socket
import time
import urllib.request
import uuid
from urllib.error import HTTPError
from urllib.parse import urlsplit

import pytest
from onvif import ONVIFCamera
from onvif.exceptions import ONVIFError
from zeep.exceptions import Fault

PROFILE = "main"
SOAP_ENV = "http://www.w3.org/2003/05/soap-envelope"


# --- raw SOAP, for what a client library hides -----------------------------------


def soap(url: str, body: str, user=None, password=None, clock_offset=dt.timedelta()):
    """POSTs one SOAP 1.2 request; returns (HTTP status, response text)."""
    header = ""
    if user is not None:
        nonce = os.urandom(16)
        created = (dt.datetime.now(dt.timezone.utc) + clock_offset).strftime("%Y-%m-%dT%H:%M:%SZ")
        digest = base64.b64encode(
            hashlib.sha1(nonce + created.encode() + password.encode()).digest()).decode()
        header = (
            '<s:Header><wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/'
            'oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/'
            'wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken>'
            f"<wsse:Username>{user}</wsse:Username>"
            '<wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-'
            f'username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>'
            '<wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-'
            f'soap-message-security-1.0#Base64Binary">{base64.b64encode(nonce).decode()}'
            f"</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken>"
            "</wsse:Security></s:Header>"
        )
    envelope = (
        f'<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="{SOAP_ENV}" '
        'xmlns:tds="http://www.onvif.org/ver10/device/wsdl">'
        f"{header}<s:Body>{body}</s:Body></s:Envelope>"
    )
    request = urllib.request.Request(
        f"{url}/onvif/device_service", data=envelope.encode(), method="POST",
        headers={"Content-Type": "application/soap+xml; charset=utf-8"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, response.read().decode()
    except HTTPError as error:
        return error.code, error.read().decode()


@pytest.fixture(scope="module")
def clock_offset(onvif_url):
    """Camera clock minus host clock: a PasswordDigest carries a timestamp
    that must be fresh by the camera's clock."""
    status, text = soap(onvif_url, "<tds:GetSystemDateAndTime/>")
    assert status == 200, text
    utc = re.search(r"UTCDateTime>(.*?)</(?:\w+:)?UTCDateTime", text, re.S).group(1)
    field = {k: int(re.search(rf"{k}>(\d+)<", utc).group(1))
             for k in ("Year", "Month", "Day", "Hour", "Minute", "Second")}
    camera_now = dt.datetime(field["Year"], field["Month"], field["Day"], field["Hour"],
                             field["Minute"], field["Second"], tzinfo=dt.timezone.utc)
    return camera_now - dt.datetime.now(dt.timezone.utc)


# --- onvif-zeep-async --------------------------------------------------------------


@pytest.fixture(scope="module")
def loop():
    loop = asyncio.new_event_loop()
    yield loop
    loop.close()


@pytest.fixture(scope="module")
def cam(loop, onvif_url, onvif_user, onvif_password):
    parts = urlsplit(onvif_url)

    async def connect():
        return ONVIFCamera(parts.hostname, parts.port, onvif_user, onvif_password,
                           nat_override=True, adjust_time=True)

    camera = loop.run_until_complete(connect())
    loop.run_until_complete(camera.update_xaddrs())
    yield camera
    loop.run_until_complete(camera.close())


@pytest.fixture(scope="module")
def devicemgmt(loop, cam):
    return loop.run_until_complete(cam.create_devicemgmt_service())


@pytest.fixture(scope="module")
def media(loop, cam):
    return loop.run_until_complete(cam.create_media_service())


@pytest.fixture(scope="module")
def ptz(loop, cam):
    return loop.run_until_complete(cam.create_ptz_service())


def status(loop, ptz):
    return loop.run_until_complete(ptz.GetStatus({"ProfileToken": PROFILE}))


def wait_idle(loop, ptz, timeout=30):
    deadline = time.monotonic() + timeout
    while status(loop, ptz).MoveStatus.PanTilt != "IDLE":
        assert time.monotonic() < deadline, "PTZ still moving"
        time.sleep(0.5)


# --- discovery ----------------------------------------------------------------------


def test_unicast_probe_gets_a_probe_match(discovery_port):
    message_id = f"uuid:{uuid.uuid4()}"
    probe = (
        f'<?xml version="1.0" encoding="UTF-8"?><e:Envelope xmlns:e="{SOAP_ENV}" '
        'xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" '
        'xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" '
        'xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Header>'
        f"<w:MessageID>{message_id}</w:MessageID>"
        '<w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>'
        '<w:Action e:mustUnderstand="true">'
        "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>"
        "<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>"
        "</e:Envelope>"
    )
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.settimeout(3)
        reply = None
        # UDP: a datagram sent before the forward is live is simply lost.
        for _ in range(10):
            sock.sendto(probe.encode(), ("127.0.0.1", discovery_port))
            try:
                reply = sock.recvfrom(65536)[0].decode()
                break
            except TimeoutError:
                continue
    assert reply, "no ProbeMatch"
    assert "ProbeMatches" in reply
    assert message_id in reply  # RelatesTo
    assert "NetworkVideoTransmitter" in reply
    xaddrs = re.search(r"XAddrs>(.*?)<", reply).group(1).split()
    # The guest's own address: that is where an NVR on its LAN would connect.
    assert any(re.fullmatch(r"http://[\d.]+:8000/onvif/device_service", x) for x in xaddrs), xaddrs


# --- device service ---------------------------------------------------------------


# The client's `authless_` prefix sends a call without the UsernameToken.


def test_system_date_and_time_needs_no_auth(loop, devicemgmt):
    answer = loop.run_until_complete(devicemgmt.authless_GetSystemDateAndTime())
    # The camera has no RTC; whatever its clock says, it says it unasked.
    assert answer.UTCDateTime.Date.Year >= 1970


def test_device_information_needs_auth(loop, onvif_url, devicemgmt):
    with pytest.raises((Fault, ONVIFError)):
        loop.run_until_complete(devicemgmt.authless_GetDeviceInformation())
    code, text = soap(onvif_url, "<tds:GetDeviceInformation/>")
    assert code == 401 and "Fault" in text and "authentication required" in text


def test_wrong_password_is_a_fault(onvif_url, onvif_user, clock_offset):
    code, text = soap(onvif_url, "<tds:GetDeviceInformation/>", onvif_user, "wrong-password",
                      clock_offset)
    assert code == 401 and "Fault" in text


def test_device_information_with_digest(loop, devicemgmt):
    info = loop.run_until_complete(devicemgmt.GetDeviceInformation())
    assert info.Manufacturer == "Anyka"
    assert info.Model and info.FirmwareVersion


def test_raw_digest_accepted(onvif_url, onvif_user, onvif_password, clock_offset):
    code, text = soap(onvif_url, "<tds:GetDeviceInformation/>", onvif_user, onvif_password,
                      clock_offset)
    assert code == 200 and "Anyka" in text


def test_capabilities_advertise_media_and_ptz(loop, devicemgmt):
    caps = loop.run_until_complete(devicemgmt.GetCapabilities({"Category": "All"}))
    assert urlsplit(caps.Media.XAddr).port == 8000
    assert caps.PTZ is not None
    assert urlsplit(caps.PTZ.XAddr).port == 8000


def test_unknown_action_is_a_sender_fault(onvif_url, onvif_user, onvif_password, clock_offset):
    code, text = soap(onvif_url, "<tds:NoSuchAction/>", onvif_user, onvif_password, clock_offset)
    assert code == 400
    assert "Sender" in text and "unsupported action: NoSuchAction" in text


# --- media service -----------------------------------------------------------------


def test_profile_with_ptz_configuration(loop, media):
    profiles = loop.run_until_complete(media.GetProfiles())
    assert [p.token for p in profiles] == [PROFILE]
    assert profiles[0].VideoEncoderConfiguration.Encoding == "H264"
    assert profiles[0].PTZConfiguration.token == "ptz_config"


def test_stream_uri(loop, media):
    answer = loop.run_until_complete(media.GetStreamUri({
        "ProfileToken": PROFILE,
        "StreamSetup": {"Stream": "RTP-Unicast", "Transport": {"Protocol": "RTSP"}},
    }))
    assert re.fullmatch(r"rtsp://[\d.]+:554/vs0", answer.Uri), answer.Uri


def test_snapshot_uri(loop, media):
    answer = loop.run_until_complete(media.GetSnapshotUri({"ProfileToken": PROFILE}))
    assert re.fullmatch(r"http://[\d.]+(:80)?/api/snapshot\.jpg", answer.Uri), answer.Uri


# --- PTZ service -------------------------------------------------------------------


def test_continuous_move_and_stop(loop, ptz):
    before = status(loop, ptz).Position.PanTilt.x
    loop.run_until_complete(ptz.ContinuousMove({
        "ProfileToken": PROFILE,
        "Velocity": {"PanTilt": {"x": 0.5, "y": 0}},
        "Timeout": dt.timedelta(seconds=5),
    }))
    moving = status(loop, ptz)
    assert moving.MoveStatus.PanTilt == "MOVING"
    time.sleep(1)
    loop.run_until_complete(ptz.Stop({"ProfileToken": PROFILE, "PanTilt": True}))
    stopped = status(loop, ptz)
    assert stopped.MoveStatus.PanTilt == "IDLE"
    assert stopped.Position.PanTilt.x > before


def test_presets(loop, ptz):
    token = loop.run_until_complete(ptz.SetPreset({"ProfileToken": PROFILE,
                                                   "PresetName": "itest-gate"}))
    saved = status(loop, ptz).Position.PanTilt
    presets = loop.run_until_complete(ptz.GetPresets({"ProfileToken": PROFILE}))
    assert "itest-gate" in [p.Name for p in presets if p.token == token]

    loop.run_until_complete(ptz.AbsoluteMove({
        "ProfileToken": PROFILE, "Position": {"PanTilt": {"x": -0.5, "y": -0.5}}}))
    wait_idle(loop, ptz)
    loop.run_until_complete(ptz.GotoPreset({"ProfileToken": PROFILE, "PresetToken": token}))
    wait_idle(loop, ptz)
    back = status(loop, ptz).Position.PanTilt
    assert abs(back.x - saved.x) < 0.01 and abs(back.y - saved.y) < 0.01

    loop.run_until_complete(ptz.RemovePreset({"ProfileToken": PROFILE, "PresetToken": token}))
    presets = loop.run_until_complete(ptz.GetPresets({"ProfileToken": PROFILE}))
    assert token not in [p.token for p in presets or []]


def test_web_and_onvif_drive_the_same_motor(loop, ptz, web, admin_token):
    # A move from the web UI is what ONVIF reports...
    assert web.post("/api/ptz/move", {"pan": 0, "tilt": 1, "timeout_ms": 5000},
                    token=admin_token).status == 204
    assert status(loop, ptz).MoveStatus.PanTilt == "MOVING"
    # ...and an ONVIF Stop ends it for the web UI too (latest command wins).
    loop.run_until_complete(ptz.Stop({"ProfileToken": PROFILE, "PanTilt": True}))
    assert web.get("/api/ptz", token=admin_token).json["moving"] is False
