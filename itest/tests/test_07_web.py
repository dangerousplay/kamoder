"""web: the admin UI's JSON API, from the host through a forwarded port.

Every forwarded connection reaches the camera from the same address (QEMU's
gateway, 10.0.2.2), so the per-IP login throttle sees the whole suite as one
client: the run logs in as admin once (`admin_token`), keeps failed logins
below the limit until the throttle scenario, which runs last and then
restarts web to forget it.

PTZ runs against the simulated head (firmware-init exports
ANYKA_ONVIF_PTZ=sim); video is absent in the VM, so snapshots are 503.
"""

import time

import pytest

from conftest import RUST

SERVICES = ("wdt_feeder", "wifi", "sshd", "web", "onvif")
VIEWER = "itest-viewer"
VIEWER_PASSWORD = "viewer-pass-1"


# --- provisioning and basics -------------------------------------------------


def test_admin_provisioned_from_camera_conf(camera):
    users = camera.check(f"cat {RUST}/etc/web-users")
    assert users.startswith("admin:admin:pbkdf2-sha256:")
    assert camera.ok(f"grep -qx 'web_admin_password=' {RUST}/camera.conf")


def test_onvif_secret_provisioned_from_camera_conf(camera, onvif_password):
    assert camera.check(f"cat {RUST}/etc/onvif.secret") == f"onvif:{onvif_password}"
    assert camera.ok(f"grep -qx 'onvif_password=' {RUST}/camera.conf")


def test_health_needs_no_login(web):
    response = web.get("/api/health")
    assert response.status == 200
    assert response.json["ok"] is True


def test_spa_is_served(web):
    index = web.get("/")
    assert index.status == 200, index
    assert index.headers["Content-Type"].startswith("text/html")
    assert '<div id="root"' in index.text
    # Client-side routes survive a reload.
    assert web.get("/logs").text == index.text


def test_unknown_api_path_is_json_404(web):
    response = web.get("/api/no-such-thing")
    assert response.status == 404 and response.code == "not_found"


# --- sessions --------------------------------------------------------------------


def test_admin_login(web, admin_token):
    me = web.get("/api/me", token=admin_token)
    assert me.status == 200, me
    assert me.json["user"] == "admin" and me.json["role"] == "admin"


def test_wrong_password_is_401(web):
    response = web.login("admin", "not-the-password")
    assert response.status == 401 and response.code == "auth"


def test_no_token_is_401(web):
    for path in ("/api/me", "/api/services", "/api/config", "/api/logs"):
        response = web.get(path)
        assert response.status == 401 and response.code == "auth", path


def test_forged_token_is_401(web, admin_token):
    header, payload, _ = admin_token.split(".")
    response = web.get("/api/me", token=f"{header}.{payload}.AAAA")
    assert response.status == 401


# --- users -------------------------------------------------------------------------


@pytest.fixture(scope="module")
def viewer_token(web, admin_token):
    created = web.post("/api/users", {"name": VIEWER, "password": VIEWER_PASSWORD,
                                      "role": "viewer"}, token=admin_token, timeout=120)
    assert created.status == 201, created
    assert created.json == {"name": VIEWER, "role": "viewer"}
    # A successful login also clears the failure counted above.
    return web.token(VIEWER, VIEWER_PASSWORD)


def test_viewer_reads_but_cannot_change(web, viewer_token):
    assert web.get("/api/services", token=viewer_token).status == 200
    assert web.get("/api/ptz", token=viewer_token).status == 200
    for method, path, body in (
        ("GET", "/api/users", None),
        ("POST", "/api/services/sshd/restart", None),
        ("PUT", "/api/config", {"values": {"log_level": "debug"}}),
        ("POST", "/api/ptz/move", {"pan": 1, "tilt": 0}),
    ):
        response = web.request(method, path, body, token=viewer_token)
        assert response.status == 403 and response.code == "forbidden", (path, response)


def test_users_listed(web, admin_token, viewer_token):
    users = web.get("/api/users", token=admin_token).json
    assert {"name": "admin", "role": "admin"} in users
    assert {"name": VIEWER, "role": "viewer"} in users


def test_last_admin_cannot_be_removed_or_demoted(web, admin_token):
    removed = web.delete("/api/users/admin", token=admin_token)
    assert removed.status == 409 and removed.code == "last_admin", removed
    demoted = web.put("/api/users/admin", {"role": "viewer"}, token=admin_token)
    assert demoted.status == 409 and demoted.code == "last_admin", demoted
    assert web.get("/api/me", token=admin_token).status == 200


def test_duplicate_user_is_409(web, admin_token, viewer_token):
    response = web.post("/api/users", {"name": VIEWER, "password": "x" * 10, "role": "viewer"},
                        token=admin_token, timeout=120)
    assert response.status == 409


def test_own_password_change(web, viewer_token):
    wrong = web.put("/api/users/me/password", {"current": "nope", "new": "viewer-pass-2"},
                    token=viewer_token, timeout=120)
    assert wrong.status == 403 and wrong.code == "wrong_password", wrong
    changed = web.put("/api/users/me/password",
                      {"current": VIEWER_PASSWORD, "new": "viewer-pass-2"},
                      token=viewer_token, timeout=120)
    assert changed.status == 200, changed
    # The change ends the old session and hands back a new one.
    assert web.get("/api/me", token=viewer_token).status == 401
    me = web.get("/api/me", token=changed.json["token"])
    assert me.status == 200 and me.json["user"] == VIEWER


def test_deleted_user_is_gone(web, admin_token):
    assert web.delete(f"/api/users/{VIEWER}", token=admin_token).status == 204
    assert VIEWER not in [u["name"] for u in web.get("/api/users", token=admin_token).json]
    assert web.delete(f"/api/users/{VIEWER}", token=admin_token).status == 404


# --- services ----------------------------------------------------------------------


def services(web, token):
    response = web.get("/api/services", token=token)
    assert response.status == 200, response
    return {s["name"]: s for s in response.json}


def test_services_listed(web, admin_token):
    listed = services(web, admin_token)
    for name in SERVICES:
        assert listed[name]["state"] == "running", listed[name]
        assert listed[name]["pid"]
    assert listed["wdt_feeder"]["critical"] is True
    assert not any(listed[name]["critical"] for name in SERVICES if name != "wdt_feeder")
    assert listed["sshd"]["log"] == "sshd.log"


def test_restart_sshd_gets_a_new_process(camera, web, admin_token):
    before = camera.pid("sshd")
    response = web.post("/api/services/sshd/restart", token=admin_token)
    assert response.status == 200, response
    assert response.json["name"] == "sshd"
    after = camera.wait(lambda: (p := camera.pid("sshd")) and p != before and p, 30,
                        "a new sshd")
    camera.wait(lambda: services(web, admin_token)["sshd"]["pid"] == after, 30,
                "the API to report the new pid")


def test_critical_service_cannot_be_stopped(camera, web, admin_token):
    before = camera.pid("wdt_feeder")
    for action in ("stop", "restart"):
        response = web.post(f"/api/services/wdt_feeder/{action}", token=admin_token)
        assert response.status == 409 and response.code == "critical", response
    time.sleep(2)
    assert camera.pid("wdt_feeder") == before


def test_unknown_service_is_404(web, admin_token):
    response = web.post("/api/services/nope/restart", token=admin_token)
    assert response.status == 404


# --- logs ----------------------------------------------------------------------------


def test_logs_listed(web, admin_token):
    names = [f["name"] for f in web.get("/api/logs", token=admin_token).json]
    for log in ("sshd.log", "web.log", "onvif.log", "supervisor.log"):
        assert log in names


def test_log_tail(web, admin_token):
    response = web.get("/api/logs/sshd.log?lines=3", token=admin_token)
    assert response.status == 200, response
    assert response.json["file"] == "sshd.log"
    assert 0 < len(response.json["lines"]) <= 3


def test_log_outside_the_directory_is_404(web, admin_token):
    assert web.get("/api/logs/..%2Fcamera.conf", token=admin_token).status == 404
    assert web.get("/api/logs/no-such.log", token=admin_token).status == 404


def test_log_stream_receives_appended_line(camera, web, admin_token):
    log = f"{RUST}/logs/itest.log"
    camera.check(f"echo start > {log}")
    grant = web.post("/api/logs/stream-token", token=admin_token)
    assert grant.status == 200, grant
    conn = web.connection(timeout=30)
    try:
        conn.request("GET", f"/api/logs/itest.log/stream?token={grant.json['token']}")
        response = conn.getresponse()
        assert response.status == 200
        assert response.headers["Content-Type"].startswith("text/event-stream")
        assert response.readline().startswith(b": open")
        marker = f"itest-marker-{time.monotonic_ns()}"
        camera.check(f"echo {marker} >> {log}")
        deadline = time.monotonic() + 20
        received = []
        while time.monotonic() < deadline:
            line = response.readline().decode()
            received.append(line)
            if line.strip() == f"data: {marker}":
                break
        else:
            raise AssertionError(f"marker not streamed; got {received}")
    finally:
        conn.close()
        camera.run(f"rm -f {log}")


def test_stream_url_needs_a_stream_token(web, admin_token):
    assert web.get("/api/logs/sshd.log/stream").status == 401
    assert web.get("/api/logs/sshd.log/stream?token=00112233445566778899aabbccddeeff").status == 401
    # Never the session JWT: it would leak a 12-hour credential into URLs.
    assert web.get(f"/api/logs/sshd.log/stream?token={admin_token}").status == 401


# --- config ---------------------------------------------------------------------------


def test_config_hides_secrets(camera, web, admin_token, web_password, onvif_password):
    response = web.get("/api/config", token=admin_token)
    assert response.status == 200, response
    values = response.json["values"]
    assert not [key for key in values if key.endswith("_password")]
    # Set or not depends on where the Wi-Fi scenarios left camera.conf.
    wifi_set = not camera.ok(f"grep -Eqx \"wifi_password=('')?\" {RUST}/camera.conf")
    assert ("wifi_password" in response.json["secrets_set"]) == wifi_set
    assert values["onvif_user"] == "onvif"
    for secret in ("homepass1", "newpass12", web_password, onvif_password):
        assert secret not in response.text


def test_config_round_trip(camera, web, admin_token):
    original = web.get("/api/config", token=admin_token).json["values"]["log_level"]
    try:
        response = web.put("/api/config", {"values": {"log_level": "debug"}}, token=admin_token)
        assert response.status == 200, response
        assert response.json["changed"] == ["log_level"]
        assert response.json["reboot_required"] is True
        assert web.get("/api/config", token=admin_token).json["values"]["log_level"] == "debug"
        assert camera.ok(f"grep -qx 'log_level=debug' {RUST}/camera.conf")
        # The rest of the file survives the rewrite.
        assert camera.ok(f"grep -q '^wifi_password=' {RUST}/camera.conf")
    finally:
        web.put("/api/config", {"values": {"log_level": original}}, token=admin_token)
    assert web.get("/api/config", token=admin_token).json["values"]["log_level"] == original


def test_config_refuses_unknown_and_read_only_keys(web, admin_token):
    for values in ({"no_such_key": "1"}, {"web_admin_password": "x"}, {"log_level": "a b"}):
        response = web.put("/api/config", {"values": values}, token=admin_token)
        assert response.status == 400, (values, response)


# --- ONVIF account and video ------------------------------------------------------


def test_onvif_credentials_reported(web, admin_token):
    response = web.get("/api/onvif", token=admin_token)
    assert response.status == 200, response
    assert response.json == {"user": "onvif", "configured": True}


def test_snapshot_without_video_is_503(web, admin_token):
    response = web.get("/api/snapshot.jpg", token=admin_token)
    assert response.status == 503 and response.code == "video_unavailable", response


# --- PTZ (simulated head, owned by onvif) ---------------------------------------


def ptz(web, token):
    response = web.get("/api/ptz", token=token)
    assert response.status == 200, response
    return response.json


def test_ptz_available(web, admin_token):
    status = ptz(web, admin_token)
    assert status["available"] is True
    assert status["backend"] == "sim"
    assert status["limits"]["pan_max_deg"] > status["limits"]["pan_min_deg"]


def test_ptz_move_and_stop(web, admin_token):
    start = ptz(web, admin_token)["pan"]
    assert web.post("/api/ptz/move", {"pan": 1, "tilt": 0, "timeout_ms": 5000},
                    token=admin_token).status == 204
    moving = ptz(web, admin_token)
    assert moving["moving"] is True
    time.sleep(1)
    assert web.post("/api/ptz/stop", token=admin_token).status == 204
    stopped = ptz(web, admin_token)
    assert stopped["moving"] is False
    assert stopped["pan"] > start
    time.sleep(1)
    assert ptz(web, admin_token)["pan"] == stopped["pan"]


def test_ptz_deadman_stops_a_forgotten_move(web, admin_token):
    assert web.post("/api/ptz/move", {"pan": -1, "tilt": 0, "timeout_ms": 500},
                    token=admin_token).status == 204
    web_camera_wait(lambda: not ptz(web, admin_token)["moving"], 10)


def test_ptz_goto_position(web, admin_token):
    assert web.post("/api/ptz/goto", {"pan": 100, "tilt": 30}, token=admin_token).status == 204
    web_camera_wait(lambda: not ptz(web, admin_token)["moving"], 30)
    status = ptz(web, admin_token)
    assert abs(status["pan"] - 100) < 0.5 and abs(status["tilt"] - 30) < 0.5


def test_ptz_presets(web, admin_token):
    saved = web.post("/api/ptz/presets", {"name": "itest-door"}, token=admin_token)
    assert saved.status == 200, saved
    preset = saved.json
    assert preset["name"] == "itest-door"
    assert preset in web.get("/api/ptz/presets", token=admin_token).json

    web.post("/api/ptz/goto", {"pan": 200, "tilt": 60}, token=admin_token)
    web_camera_wait(lambda: not ptz(web, admin_token)["moving"], 30)
    assert web.post("/api/ptz/goto", {"preset": preset["id"]}, token=admin_token).status == 204
    web_camera_wait(lambda: not ptz(web, admin_token)["moving"], 30)
    status = ptz(web, admin_token)
    assert abs(status["pan"] - preset["pan"]) < 0.5 and abs(status["tilt"] - preset["tilt"]) < 0.5

    assert web.delete(f"/api/ptz/presets/{preset['id']}", token=admin_token).status == 204
    assert preset["id"] not in [p["id"] for p in web.get("/api/ptz/presets",
                                                          token=admin_token).json]
    missing = web.post("/api/ptz/goto", {"preset": preset["id"]}, token=admin_token)
    assert missing.status == 404 and missing.code == "unknown_preset"


def test_ptz_home(web, admin_token):
    assert web.post("/api/ptz/home", token=admin_token).status == 204
    web_camera_wait(lambda: not ptz(web, admin_token)["moving"], 30)


def web_camera_wait(predicate, timeout):
    deadline = time.monotonic() + timeout
    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.5)


# --- login throttle: last, it locks this client out of logging in ------------------


def test_failed_logins_are_throttled(camera, web, admin_token, web_password):
    statuses = [web.login("itest-nobody", f"guess-{n}").status for n in range(6)]
    assert statuses[0] == 401
    assert 429 in statuses, statuses
    blocked = web.login("admin", web_password)
    # Even the right password: the block is per client, not per account.
    assert blocked.status == 429 and blocked.code == "rate_limited", blocked
    assert int(blocked.headers["Retry-After"]) > 0
    # Sessions already issued keep working.
    assert web.get("/api/me", token=admin_token).status == 200

    # The throttle is in memory: a restart of web forgets it, so later
    # tests (and reruns against this VM) can log in again.
    before = camera.pid("web")
    camera.check(f"kill {before}")
    camera.wait(lambda: (p := camera.pid("web")) and p != before, 60, "web restarted")
    camera.wait(web.healthy, 60, "web to answer again")
    assert web.login("admin", web_password).status == 200
