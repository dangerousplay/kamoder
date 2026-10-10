"""The card boots the Rust stack the way the camera would run it."""

from conftest import RUST


def test_services_are_running(camera):
    for comm in ("supervisor", "wdt_feeder", "wifi", "sshd"):
        assert camera.running(comm), comm


def test_rescue_telnetd_started(camera):
    # start_telnetd=1 in the test camera.conf.
    assert camera.running("telnetd")


def test_every_log_exists(camera):
    for log in ("boot.log", "init.log", "supervisor.log", "wdt_feeder.log", "wifi.log", "sshd.log"):
        camera.wait(lambda: camera.ok(f"test -s {RUST}/logs/{log}"), 30, f"{log} to be written")


def test_each_service_logs_to_its_own_file(camera):
    # Regression: services inherited ANYKA_LOG_FILE and all wrote supervisor.log.
    assert "wifi started" in camera.log("wifi.log")
    assert "wifi started" not in camera.log("supervisor.log")


def test_logs_live_on_the_sd_card(camera):
    assert camera.ok(f"test -d {RUST}/logs") and RUST.startswith("/mnt/disc1/")


def test_service_logs_have_no_terminal_escape_codes(camera):
    # Regression: once services logged to a pipe, colour codes leaked in.
    for log in ("wifi.log", "sshd.log", "wdt_feeder.log"):
        assert "\x1b[" not in camera.log(log), log
