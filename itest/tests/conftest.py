"""Fixtures for the camera system tests.

labgrid boots the VM (QEMUDriver) and gives a shell on its serial console
(ShellDriver). `Camera` wraps that shell with the handful of observations the
tests make — process table, mounts, Wi-Fi state, logs — so each test reads as
the scenario it checks rather than as shell plumbing.
"""

from __future__ import annotations

import http.client
import json
import os
import re
import shlex
import socket
import subprocess
import time
from dataclasses import dataclass
from urllib.parse import urlsplit

import pytest

RUST = "/mnt/disc1/anyka_rust"
ROM_CONF = "/rom/wpa_supplicant0.conf"

# The simulated world, mirrored from itest/guest/firmware-init.
NETWORKS = {
    "HomeNet": {"psk": "homepass1", "ap": "home", "net": "192.168.10"},
    "NewNet": {"psk": "newpass12", "ap": "new", "net": "192.168.30"},
}


@dataclass
class Wifi:
    state: str
    ssid: str | None
    address: str | None


class Camera:
    def __init__(self, shell, qemu):
        self.shell = shell
        self.qemu = qemu

    # --- shell -----------------------------------------------------------------

    def run(self, cmd: str, timeout: float = 60) -> tuple[str, int]:
        out, _, code = self.shell.run(cmd, timeout=timeout)
        return "\n".join(out), code

    def check(self, cmd: str, timeout: float = 60) -> str:
        out, code = self.run(cmd, timeout)
        assert code == 0, f"`{cmd}` exited {code}:\n{out}"
        return out

    def ok(self, cmd: str, timeout: float = 60) -> bool:
        return self.run(cmd, timeout)[1] == 0

    def wait(self, predicate, timeout: float, what: str, interval: float = 2):
        """Polls `predicate` until it returns something truthy."""
        deadline = time.monotonic() + timeout
        last = None
        while True:
            last = predicate()
            if last:
                return last
            if time.monotonic() > deadline:
                raise AssertionError(f"timed out after {timeout:.0f}s waiting for {what}")
            time.sleep(interval)

    def read(self, path: str) -> str:
        out, code = self.run(f"cat {shlex.quote(path)} 2>/dev/null")
        return out if code == 0 else ""

    # --- processes ---------------------------------------------------------------

    def pid(self, comm: str) -> int | None:
        """First pid whose /proc/<pid>/comm is `comm` (capped at 15 chars)."""
        out, _ = self.run(
            "for c in /proc/[0-9]*/comm; do read -r n < $c 2>/dev/null || continue; "
            f"if [ \"$n\" = {shlex.quote(comm[:15])} ]; then p=${{c#/proc/}}; echo ${{p%/comm}}; break; fi; done"
        )
        return int(out) if out.strip().isdigit() else None

    def running(self, comm: str) -> bool:
        return self.pid(comm) is not None

    # --- Wi-Fi -------------------------------------------------------------------

    def wifi(self) -> Wifi:
        status, _ = self.run("wpa_cli -p /etc/Wireless -i wlan0 status 2>/dev/null")
        fields = dict(line.split("=", 1) for line in status.splitlines() if "=" in line)
        addr, _ = self.run(
            "ifconfig wlan0 2>/dev/null | sed -n 's/.*inet addr:\\([0-9.]*\\).*/\\1/p'"
        )
        return Wifi(fields.get("wpa_state", "NONE"), fields.get("ssid"), addr.strip() or None)

    def rom_ssid(self) -> str | None:
        m = re.search(r'^\s*ssid="(.*)"', self.read(ROM_CONF), re.M)
        return m.group(1) if m else None

    def connected_to(self, ssid: str) -> bool:
        """Configured, associated, addressed from that AP, gateway answering."""
        net = NETWORKS[ssid]["net"]
        wifi = self.wifi()
        return (
            self.rom_ssid() == ssid
            and wifi.state == "COMPLETED"
            and wifi.ssid == ssid
            and (wifi.address or "").startswith(net + ".")
            and self.ok(f"ping -c 1 -W 2 {net}.1 >/dev/null 2>&1")
        )

    def wait_connected(self, ssid: str, timeout: float = 120) -> None:
        self.wait(lambda: self.connected_to(ssid), timeout, f"camera connected to {ssid}")

    def ap(self, name: str, action: str) -> None:
        self.check(f"ap {name} {action}")

    def external_switch(self, ssid: str, psk: str) -> None:
        """What /ipc/ipc (or a person on the console) does: rewrite the
        station config and restart the station with it."""
        conf = (
            "ctrl_interface=/etc/Wireless\nupdate_config=1\nnetwork={\n"
            f'    ssid="{ssid}"\n    scan_ssid=1\n    key_mgmt=WPA-PSK\n    psk="{psk}"\n}}\n'
        )
        self.check(f"printf %s {shlex.quote(conf)} > {ROM_CONF}")
        self.check(
            "killall wpa_supplicant udhcpc 2>/dev/null; sleep 1; rm -f /etc/Wireless/wlan0; "
            f"wpa_supplicant -B -iwlan0 -Dwext -c {ROM_CONF} -C /etc/Wireless >/dev/null 2>&1; "
            "udhcpc -i wlan0 -b -s /usr/share/udhcpc/default.script >/dev/null 2>&1"
        )

    # --- the card ----------------------------------------------------------------

    def set_conf(self, key: str, value: str) -> None:
        self.check(f"sed -i \"s/^{key}=.*/{key}='{value}'/\" {RUST}/camera.conf")

    def log(self, name: str) -> str:
        return self.read(f"{RUST}/logs/{name}")


class LogCursor:
    """The part of a log written since the cursor was made."""

    def __init__(self, camera: Camera, name: str):
        self.camera, self.name = camera, name
        self.start = len(camera.log(name))

    def new(self) -> str:
        return self.camera.log(self.name)[self.start:]

    def actions(self) -> list[tuple[str, str | None]]:
        """wifi's reconcile actions, as (action, target ssid)."""
        found = []
        for line in self.new().splitlines():
            m = re.search(r'action="(\w+)"(?: target=Some\("([^"]*)"\))?', line)
            if m and "reconcile" in line:
                found.append((m.group(1), m.group(2)))
        return found


class Response:
    def __init__(self, status: int, headers, body: bytes):
        self.status, self.headers, self.body = status, headers, body

    @property
    def json(self):
        return json.loads(self.body)

    @property
    def text(self) -> str:
        return self.body.decode(errors="replace")

    @property
    def code(self) -> str | None:
        """The API's machine-readable error tag (`{"error", "code"}`)."""
        try:
            return self.json.get("code")
        except ValueError:
            return None

    def __repr__(self):
        return f"<{self.status} {self.body[:300]!r}>"


class Web:
    """web's JSON API, through the forwarded port."""

    def __init__(self, url: str):
        parts = urlsplit(url)
        self.url, self.host, self.port = url, parts.hostname, parts.port

    def connection(self, timeout: float = 60) -> http.client.HTTPConnection:
        return http.client.HTTPConnection(self.host, self.port, timeout=timeout)

    def request(self, method: str, path: str, body=None, token: str | None = None,
                timeout: float = 60) -> Response:
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        conn = self.connection(timeout)
        try:
            conn.request(method, path, body=data, headers=headers)
            response = conn.getresponse()
            return Response(response.status, response.headers, response.read())
        finally:
            conn.close()

    def get(self, path, token=None, **kw) -> Response:
        return self.request("GET", path, token=token, **kw)

    def post(self, path, body=None, token=None, **kw) -> Response:
        return self.request("POST", path, body, token=token, **kw)

    def put(self, path, body=None, token=None, **kw) -> Response:
        return self.request("PUT", path, body, token=token, **kw)

    def delete(self, path, token=None, **kw) -> Response:
        return self.request("DELETE", path, token=token, **kw)

    def login(self, user: str, password: str) -> Response:
        # PBKDF2 at 20 000 iterations on an emulated ARM926: seconds, not ms.
        return self.post("/api/auth/login", {"user": user, "password": password}, timeout=120)

    def token(self, user: str, password: str) -> str:
        response = self.login(user, password)
        assert response.status == 200, response
        return response.json["token"]

    def healthy(self) -> bool:
        try:
            return self.get("/api/health", timeout=5).status == 200
        except OSError:
            return False


# --- fixtures -------------------------------------------------------------------------


@pytest.fixture(scope="session")
def camera(target) -> Camera:
    qemu = target.get_driver("QEMUDriver")
    target.activate(qemu)
    qemu.on()
    shell = target.get_driver("ShellDriver")
    target.activate(shell)
    cam = Camera(shell, qemu)
    # The console is up before the stack is; everything below needs the
    # supervisor's services running.
    cam.wait(lambda: cam.running("wifi") and cam.running("sshd"), 180,
             "the Rust stack to start")
    yield cam
    qemu.off()


def free_port(kind: int = socket.SOCK_STREAM) -> int:
    with socket.socket(socket.AF_INET, kind) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def forward(camera: Camera, proto: str, guest_port: int) -> int:
    """A host port on 127.0.0.1 forwarded to `guest_port` on eth0. QEMU user
    networking: the guest sees every forwarded connection come from the
    gateway, 10.0.2.2."""
    port = free_port(socket.SOCK_DGRAM if proto == "udp" else socket.SOCK_STREAM)
    camera.qemu.add_port_forward(proto, "127.0.0.1", port, "", guest_port)
    return port


@pytest.fixture(scope="session")
def ssh_port(camera) -> int:
    return forward(camera, "tcp", 2222)


@pytest.fixture(scope="session")
def web_url(camera) -> str:
    url = f"http://127.0.0.1:{forward(camera, 'tcp', 80)}"
    web = Web(url)
    camera.wait(web.healthy, 120, "web to answer")
    return url


@pytest.fixture(scope="session")
def web(web_url) -> Web:
    return Web(web_url)


@pytest.fixture(scope="session")
def web_password() -> str:
    return os.environ["LG_ITEST_WEB_PASSWORD"]


@pytest.fixture(scope="session")
def admin_token(web, web_password) -> str:
    """One admin session for the whole run: logins are rate limited per
    client IP, and every forwarded connection has the same one."""
    return web.token("admin", web_password)


@pytest.fixture(scope="session")
def onvif_url(camera) -> str:
    port = forward(camera, "tcp", 8000)

    def answering():
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                # slirp accepts on the host side before the guest does; a
                # closed guest port shows as an immediate EOF.
                s.sendall(b"GET / HTTP/1.0\r\n\r\n")
                return s.recv(1) != b""
        except OSError:
            return False

    camera.wait(answering, 120, "onvif to answer")
    return f"http://127.0.0.1:{port}"


@pytest.fixture(scope="session")
def onvif_user() -> str:
    return "onvif"


@pytest.fixture(scope="session")
def onvif_password() -> str:
    return os.environ["LG_ITEST_ONVIF_PASSWORD"]


@pytest.fixture(scope="session")
def discovery_port(camera) -> int:
    return forward(camera, "udp", 3702)


@pytest.fixture(scope="session")
def password() -> str:
    return os.environ["LG_ITEST_PASSWORD"]


def ssh_args(port: int, command: str, tty: bool = False) -> list[str]:
    """argv for a password login through sshpass; the password goes in $SSHPASS."""
    args = [
        "sshpass", "-e", "ssh",
        "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=30",
        "-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no",
        "-o", "NumberOfPasswordPrompts=1", "-p", str(port),
    ]
    if tty:
        args.append("-tt")
    return args + ["root@127.0.0.1", command]


def ssh(port: int, password: str, command: str, tty: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(ssh_args(port, command, tty), env={**os.environ, "SSHPASS": password},
                          capture_output=True, text=True, timeout=120, stdin=subprocess.DEVNULL)
