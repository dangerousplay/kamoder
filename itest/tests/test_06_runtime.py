"""Every binary on the card loads its libraries from the card.

The camera's ld.so is the firmware's own (uClibc-ng 1.0.31). Only `libc.so.0`
and the loader itself may come from the firmware; the libc stubs, the GCC
runtime and the SDK objects come from `lib/sys` and `lib/<variant>` through
each object's RPATH, with no LD_LIBRARY_PATH and nothing in /tmp.
"""

import shlex

import pytest

from conftest import RUST

RUNTIME = ("libpthread.so.0", "libm.so.0", "librt.so.0", "libdl.so.0", "libgcc_s.so.1", "libstdc++.so.6")
FIRMWARE = ("libc.so.0", "ld-uClibc.so.0", "ld-uClibc.so.1")


def binaries(camera) -> list[str]:
    # One name per line. The serial console also carries kernel and
    # supervisor log lines (`[14:03:07] [  281.48] …`) that can interleave with the
    # output; no binary name starts with `[`.
    out = camera.check(f'for f in {RUST}/bin/*; do echo "${{f##*/}}"; done')
    return [name for name in out.splitlines() if name and not name.startswith("[")]


def test_runtime_libraries_are_on_the_card(camera):
    for lib in RUNTIME:
        assert camera.ok(f"test -f {RUST}/lib/sys/{lib}"), lib


def test_init_sets_up_nothing_in_tmp(camera):
    assert not camera.ok("test -e /tmp/lib/libpthread.so.0")


def test_every_library_resolves_from_the_card(camera):
    for binary in binaries(camera):
        out, code = camera.run(f"env -i LD_TRACE_LOADED_OBJECTS=1 {RUST}/bin/{binary}")
        assert code == 0, f"{binary}:\n{out}"
        assert "not found" not in out, f"{binary}:\n{out}"
        for line in out.splitlines():
            if "=>" not in line:
                continue
            name, path = (part.strip() for part in line.split("=>", 1))
            if name in FIRMWARE:
                continue
            assert path.startswith(f"{RUST}/"), f"{binary}: {name} from {path}"


def test_every_binary_starts_with_an_empty_environment(camera):
    # LD_BIND_NOW: an unresolved function fails here, not on first call.
    for binary in binaries(camera):
        out, code = camera.run(f"env -i LD_BIND_NOW=1 {RUST}/bin/{binary} --help")
        assert code == 0, f"{binary} exited {code}:\n{out}"


@pytest.mark.parametrize("comm", ["supervisor", "wdt_feeder", "sshd", "web", "onvif"])
def test_running_services_map_only_card_and_libc(camera, comm):
    pid = camera.pid(comm)
    assert pid, f"{comm} is not running"
    maps = camera.check(f"cat /proc/{pid}/maps")
    for line in maps.splitlines():
        path = line.split()[-1] if len(line.split()) >= 6 else ""
        name = path.rsplit("/", 1)[-1]
        # Device mappings (/dev/uio0, ...) and anonymous ones are not libraries.
        if ".so" not in name or path.startswith(RUST + "/"):
            continue
        assert path.startswith("/lib/") and (
            name.startswith("libuClibc-") or name.startswith("ld-uClibc")
        ), f"{comm} maps {shlex.quote(path)}"
