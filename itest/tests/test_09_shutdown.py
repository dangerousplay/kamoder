"""An orderly stop leaves nothing running and the watchdog disarmed."""

from conftest import RUST


def test_stop_takes_everything_down(camera):
    camera.check(f"sh {RUST}/init.sh --stop", timeout=90)
    for comm in ("supervisor", "sshd", "wifi", "web", "onvif",
                 "wdt_feeder"):
        camera.wait(lambda: not camera.running(comm), 30, f"{comm} to exit")


def test_watchdog_disarmed_on_stop(camera):
    # wdt_feeder programs an 8 s timeout. Without the magic close on SIGTERM,
    # softdog resets the VM during this sleep and the shell stops answering.
    camera.check("sleep 15", timeout=60)
    assert camera.check("echo alive") == "alive"
