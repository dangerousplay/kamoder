"""The hardware watchdog is always fed."""


def test_feeder_holds_the_watchdog(camera):
    pid = camera.pid("wdt_feeder")
    assert pid
    assert "/dev/watchdog" in camera.check(f"ls -l /proc/{pid}/fd")


def test_killed_feeder_is_restarted_before_the_board_resets(camera):
    before = camera.pid("wdt_feeder")
    camera.check(f"kill -9 {before}")
    camera.wait(lambda: (p := camera.pid("wdt_feeder")) and p != before, 15,
                "the supervisor to restart wdt_feeder")
