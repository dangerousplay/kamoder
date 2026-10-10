#!/usr/bin/env python3
"""Push a file to the camera in resumable chunks over a Wi-Fi link that drops.

The camera cannot open connections towards the build host, so every transfer is
the host connecting to a `nc -l` the camera has just been told to start. On a
stable link one `nc` would do. This one is not stable — it goes unreachable for
minutes at a time — so the file is split, each chunk is pushed and size-checked
independently, and a rerun skips the chunks already on the card.

    tools/push.py /tmp/v8pay.tgz /mnt/disc1/v8pay.tgz
"""

from __future__ import annotations

import argparse
import socket
import sys
import time

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from cam import Camera, HOST, PORT  # noqa: E402

CHUNK = 128 * 1024
DATA_PORT = 5566


def push_chunk(host: str, port: int, payload: bytes, attempts: int = 4) -> None:
    """Connect to the listener the camera just started and send one chunk."""
    last = None
    for _ in range(attempts):
        try:
            with socket.create_connection((host, port), timeout=20.0) as data:
                data.settimeout(60.0)
                data.sendall(payload)
                data.shutdown(socket.SHUT_WR)
                return
        except OSError as err:
            last = err
            time.sleep(2.0)
    raise OSError(f"data connection failed: {last}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source")
    parser.add_argument("destination")
    parser.add_argument("--host", default=HOST)
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--data-port", type=int, default=DATA_PORT)
    parser.add_argument("--chunk", type=int, default=CHUNK)
    args = parser.parse_args()

    blob = open(args.source, "rb").read()
    chunks = [blob[i : i + args.chunk] for i in range(0, len(blob), args.chunk)]
    print(f"{len(blob)} bytes in {len(chunks)} chunks of {args.chunk}")

    parts = f"{args.destination}.parts"
    for index, chunk in enumerate(chunks):
        name = f"{parts}/{index:03d}"
        while True:
            cam = Camera(args.host, args.port, connect_timeout=600.0)
            try:
                cam.run(f"mkdir -p {parts}")
                # `wc -c` rather than `stat`: BusyBox here is built without it.
                have = cam.run(f"wc -c < {name} 2>/dev/null || echo 0").strip()
                if have.isdigit() and int(have) == len(chunk):
                    print(f"chunk {index:03d} already there")
                    break

                # The listener must be running before the host connects, and
                # `nc -l` exits after one connection, so it is started per chunk.
                cam.run(f"rm -f {name}; nc -l -p {args.data_port} > {name} &", timeout=10)
                time.sleep(1.0)
                push_chunk(args.host, args.data_port, chunk)
                time.sleep(1.0)
                have = cam.run(f"wc -c < {name} 2>/dev/null || echo 0").strip()
                if have.isdigit() and int(have) == len(chunk):
                    print(f"chunk {index:03d} ok ({len(chunk)} bytes)")
                    break
                print(f"chunk {index:03d} short: got {have!r}, retrying")
            except OSError as err:
                print(f"chunk {index:03d} failed: {err}; retrying")
            finally:
                cam.close()
            time.sleep(3.0)

    cam = Camera(args.host, args.port, connect_timeout=600.0)
    try:
        cam.run(f"cat {parts}/* > {args.destination} && rm -rf {parts}", timeout=120)
        size = cam.run(f"wc -c < {args.destination}").strip()
        print(f"assembled {args.destination}: {size} bytes (expected {len(blob)})")
    finally:
        cam.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
