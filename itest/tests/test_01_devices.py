"""The simulated camera looks like the camera, before anything of ours runs."""

import pytest

pytestmark = pytest.mark.vm


def mounts(camera):
    return camera.read("/proc/mounts")


def test_sd_card_is_fat_on_mmcblk0(camera):
    assert "/dev/mmcblk0p1 /mnt/disc1 vfat" in mounts(camera)


def test_rom_is_jffs2_flash(camera):
    assert " /rom jffs2 " in mounts(camera)


def test_app_partition_on_ipc_and_usr(camera):
    assert " /ipc squashfs " in mounts(camera)
    # The firmware's own Wi-Fi tools, not test doubles.
    for tool in ("wpa_supplicant", "wpa_cli", "hostapd", "iwconfig"):
        assert camera.ok(f"test -x /usr/bin/{tool}"), tool


def test_watchdog_device(camera):
    assert camera.ok("test -c /dev/watchdog")


def test_wlan0_is_a_wireless_interface(camera):
    assert "IEEE 802.11" in camera.check("iwconfig wlan0 2>&1")


def test_both_access_points_are_on_the_air(camera):
    def seen():
        camera.run("wpa_cli -p /etc/Wireless -i wlan0 scan >/dev/null 2>&1")
        out = camera.check("wpa_cli -p /etc/Wireless -i wlan0 scan_results")
        return "HomeNet" in out and "NewNet" in out
    camera.wait(seen, 60, "both SSIDs in scan results", interval=5)


def test_firmware_joined_homenet(camera):
    camera.wait_connected("HomeNet", timeout=120)
