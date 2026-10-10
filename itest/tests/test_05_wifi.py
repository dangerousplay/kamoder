"""wifi against a real (simulated) radio.

The camera's own wpa_supplicant 2.8 associates over mac80211_hwsim to the
firmware's own hostapd 2.8 running two WPA2 access points, each in its own
network namespace with its own DHCP server and gateway. Nothing here is a test
double: authentication, DHCP and the gateway ARP check all happen over the
simulated air.

Timers are shortened in firmware-init: 2 s passes, 20 s grace, 5-10 s
backoff, rollback after 2 failed retries, 90 s hold.

The scenarios run in order on one VM; each starts by stating the state it
needs.
"""

import pytest

from conftest import NETWORKS, RUST, LogCursor

pytestmark = pytest.mark.vm

GOOD = f"{RUST}/backup/wpa_supplicant0.conf.good"


def desire(camera, ssid: str, psk: str | None = None) -> None:
    """Edit camera.conf the way a user would."""
    camera.set_conf("wifi_ssid", ssid)
    camera.set_conf("wifi_password", NETWORKS[ssid]["psk"] if psk is None and ssid else psk or "")


def good_is(camera, ssid: str) -> bool:
    return f'ssid="{ssid}"' in camera.read(GOOD)


def test_steady_state_records_good_and_changes_nothing(camera):
    camera.wait_connected("HomeNet")
    camera.wait(lambda: good_is(camera, "HomeNet"), 60, "HomeNet recorded as last-known-good")
    wpa = camera.pid("wpa_supplicant")
    log = LogCursor(camera, "wifi.log")

    camera.check("sleep 20")

    assert camera.pid("wpa_supplicant") == wpa, "station restarted while healthy"
    assert log.actions() == []
    assert not camera.ok(f"test -e {RUST}/backup/wpa_supplicant0.conf.last"), \
        "/rom rewritten although it already matched"
    assert "healthy     true" in camera.check(f"cd {RUST} && sh run-on-camera.sh wifi status")


def test_switched_to_another_working_network_is_reconciled_back(camera):
    """The scenario: something moves the camera to a different network that
    works. wifi records it (it is a way back if HomeNet ever fails) and
    then returns the camera to the network camera.conf asks for."""
    camera.wait_connected("HomeNet")
    log = LogCursor(camera, "wifi.log")

    camera.external_switch("NewNet", NETWORKS["NewNet"]["psk"])

    camera.wait(lambda: ("apply", "HomeNet") in log.actions(), 120,
                "wifi to re-apply HomeNet")
    camera.wait_connected("HomeNet")
    actions = log.actions()
    # It was really on NewNet — associated, addressed, gateway answering —
    # because only then does it record a network as good.
    assert ("record_good", "NewNet") in actions
    assert actions.index(("record_good", "NewNet")) < actions.index(("apply", "HomeNet"))
    assert "rollback" not in [a for a, _ in actions]
    # The config it replaced is kept.
    assert 'ssid="NewNet"' in camera.read(f"{RUST}/backup/wpa_supplicant0.conf.last")
    # And it left other machines' daemons alone: both access points (hostapd
    # in their own network namespaces) are still up. Regression: it once
    # stopped every hostapd by name.
    hostapds = camera.check("grep -lx hostapd /proc/[0-9]*/comm 2>/dev/null | wc -l")
    assert int(hostapds) == 2


def test_reconcile_still_converges_when_the_last_good_record_cannot_be_saved(camera):
    """After a power cut the kernel can remount the FAT card read-only; then
    the last-good record cannot be saved. That must not stop the camera
    getting back to its network. (Regression: a failed save left the loop
    retrying `record_good` forever.)

    Userspace cannot force a read-only remount while logs are open, so a
    read-only tmpfs over backup/ produces the same EROFS for wifi's
    writes while leaving the logs readable for the assertions."""
    camera.wait_connected("HomeNet")
    backup = f"{RUST}/backup"
    camera.check(f"mount -t tmpfs -o ro,size=64k tmpfs {backup}")
    log = LogCursor(camera, "wifi.log")
    try:
        camera.external_switch("NewNet", NETWORKS["NewNet"]["psk"])
        camera.wait(lambda: ("apply", "HomeNet") in log.actions(), 150, "HomeNet re-applied")
        camera.wait_connected("HomeNet", timeout=150)
        assert "Read-only file system" in log.new(), "the save should really have failed"
    finally:
        camera.check(f"umount {backup}")


def test_switched_away_while_desired_ap_is_down_rolls_back_and_stays_reachable(camera):
    """Same switch, but HomeNet is off the air. Returning to it would strand
    the camera, so after the retries it rolls back to NewNet — the network it
    just observed working — and leaves HomeNet alone for the hold period."""
    camera.wait_connected("HomeNet")
    camera.ap("home", "down")
    log = LogCursor(camera, "wifi.log")

    camera.external_switch("NewNet", NETWORKS["NewNet"]["psk"])

    camera.wait(lambda: ("rollback", "NewNet") in log.actions(), 240,
                "rollback to NewNet")
    camera.wait_connected("NewNet")
    actions = [a for a, _ in log.actions()]
    assert ("apply", "HomeNet") in log.actions(), "it did try the desired network first"
    assert actions.count("reassociate") >= 2, actions

    # Held: no further attempts at HomeNet for a while, camera stays up.
    held = LogCursor(camera, "wifi.log")
    camera.check("sleep 30")
    assert ("apply", "HomeNet") not in held.actions()
    assert camera.connected_to("NewNet")


def test_desired_network_is_retried_after_the_hold_once_it_is_back(camera):
    camera.ap("home", "up")
    camera.wait_connected("HomeNet", timeout=240)
    camera.wait(lambda: good_is(camera, "HomeNet"), 30, "HomeNet recorded as good again")


def test_changing_camera_conf_moves_the_camera(camera):
    camera.wait_connected("HomeNet")
    orig = camera.read(f"{RUST}/backup/wpa_supplicant0.conf.orig")
    assert orig, "the first config wifi replaced is kept"

    desire(camera, "NewNet")

    camera.wait_connected("NewNet")
    camera.wait(lambda: good_is(camera, "NewNet"), 30, "NewNet recorded as good")
    # Written once, never again.
    assert camera.read(f"{RUST}/backup/wpa_supplicant0.conf.orig") == orig


def test_wrong_password_is_rolled_back(camera):
    """A typo in camera.conf: the AP rejects the 4-way handshake."""
    camera.wait_connected("NewNet")
    log = LogCursor(camera, "wifi.log")

    desire(camera, "HomeNet", "wrong-password")

    camera.wait(lambda: ("rollback", "NewNet") in log.actions(), 240, "rollback to NewNet")
    camera.wait_connected("NewNet")


def test_corrected_password_applies_without_waiting_out_the_hold(camera):
    """The hold is for the network that failed. Fixing the password is a
    different desired network and goes straight in."""
    camera.wait_connected("NewNet")
    log = LogCursor(camera, "wifi.log")

    desire(camera, "HomeNet")

    camera.wait_connected("HomeNet", timeout=90)
    assert ("apply", "HomeNet") in log.actions()


def test_with_nothing_desired_a_switch_to_a_working_network_is_adopted(camera):
    camera.wait_connected("HomeNet")
    camera.set_conf("wifi_ssid", "")
    camera.set_conf("wifi_password", "")
    camera.check("sleep 5")
    log = LogCursor(camera, "wifi.log")

    camera.external_switch("NewNet", NETWORKS["NewNet"]["psk"])

    camera.wait_connected("NewNet")
    camera.wait(lambda: good_is(camera, "NewNet"), 60, "NewNet adopted as good")
    camera.check("sleep 20")
    assert camera.connected_to("NewNet")
    assert [a for a, _ in log.actions() if a != "record_good"] == [], \
        "it changed a working network nobody asked it to change"


def test_access_point_outage_is_ridden_out(camera):
    """The AP disappears and comes back. There is nowhere better to go, so it
    retries with backoff and the camera is back once the AP is."""
    camera.wait_connected("NewNet")
    log = LogCursor(camera, "wifi.log")

    camera.ap("new", "down")
    camera.wait(lambda: "reassociate" in [a for a, _ in log.actions()], 120,
                "a reassociation attempt during the outage")
    camera.ap("new", "up")

    camera.wait_connected("NewNet", timeout=120)
    assert "rollback" not in [a for a, _ in log.actions()]
