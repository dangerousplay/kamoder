#!/usr/bin/env python3
"""Run commands on the camera over BusyBox telnetd, on a link that drops.

`cam_telnet.py` in the anyka_happy tree assumes the connection succeeds and
that the shell prompt arrives promptly. On this camera's Wi-Fi neither holds:
`connect()` times out for minutes at a stretch, and telnetd sometimes accepts a
socket and then sends nothing. So this version retries the connection, answers
the IAC option negotiation, and brackets each command with a sentinel instead
of pattern-matching the prompt — the prompt string changes with the working
directory and gets mangled by partial reads.

    tools/cam.py 'uname -a'
    tools/cam.py --timeout 300 'sh /mnt/disc1/anyka_rust/run-on-camera.sh md_demo_v8 50'
"""

from __future__ import annotations

import argparse
import re
import socket
import sys
import time

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
ANSI = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]")

HOST = "192.168.20.187"
PORT = 23
BEGIN = "__AK_BEGIN__"
END = "__AK_END__"


class Camera:
    def __init__(self, host: str = HOST, port: int = PORT, connect_timeout: float = 180.0):
        self.sock = self._connect(host, port, connect_timeout)
        self.sock.settimeout(0.5)
        self._drain(3.0)

    @staticmethod
    def _connect(host: str, port: int, budget: float) -> socket.socket:
        deadline = time.time() + budget
        last = None
        while time.time() < deadline:
            try:
                return socket.create_connection((host, port), timeout=8.0)
            except OSError as err:
                last = err
                time.sleep(3.0)
        raise SystemExit(f"cannot reach {host}:{port} within {budget:.0f}s ({last})")

    def _negotiate(self, chunk: bytes) -> bytes:
        """Strip IAC sequences, refusing every option, and return the payload."""
        out, reply, i = bytearray(), bytearray(), 0
        while i < len(chunk):
            if chunk[i] == IAC and i + 1 < len(chunk):
                cmd = chunk[i + 1]
                if cmd in (DO, DONT, WILL, WONT) and i + 2 < len(chunk):
                    opt = chunk[i + 2]
                    reply += bytes([IAC, WONT if cmd == DO else DONT, opt])
                    i += 3
                    continue
                if cmd == SB:
                    end = chunk.find(bytes([IAC, SE]), i)
                    i = len(chunk) if end < 0 else end + 2
                    continue
                i += 2
                continue
            out.append(chunk[i])
            i += 1
        if reply:
            self.sock.sendall(bytes(reply))
        return bytes(out)

    def _drain(self, seconds: float) -> bytes:
        buf, end = bytearray(), time.time() + seconds
        while time.time() < end:
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            buf += self._negotiate(chunk)
        return bytes(buf)

    def run(self, command: str, timeout: float = 60.0) -> str:
        self._drain(0.2)
        # The sentinels make the output boundaries explicit, so a slow or
        # chunked reply is still parsed correctly.
        wrapped = f"echo {BEGIN}; {command}; echo {END}$?\n"
        self.sock.sendall(wrapped.encode("latin-1"))

        buf, end = bytearray(), time.time() + timeout
        while time.time() < end:
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                if END.encode() in buf:
                    break
                continue
            except OSError:
                break
            if not chunk:
                break
            buf += self._negotiate(chunk)
            if END.encode() in buf and buf.rstrip().endswith(b"$") is False:
                # Give the status digits a moment to arrive.
                time.sleep(0.2)
                try:
                    buf += self._negotiate(self.sock.recv(4096))
                except OSError:
                    pass
                break

        text = ANSI.sub(b"", bytes(buf)).decode("latin-1", "replace").replace("\r", "")
        start = text.find(BEGIN)
        if start >= 0:
            # Skip the echoed command line that contains the sentinel.
            text = text[start + len(BEGIN):]
            text = text.split("\n", 1)[1] if "\n" in text else ""
        stop = text.find(END)
        if stop >= 0:
            text = text[:stop]
        # The echoed command itself can appear before the sentinel on slow links.
        return "\n".join(
            line for line in text.split("\n") if BEGIN not in line and END not in line
        ).strip("\n")

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="+")
    parser.add_argument("--host", default=HOST)
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--timeout", type=float, default=60.0, help="per-command seconds")
    parser.add_argument("--connect-timeout", type=float, default=180.0)
    args = parser.parse_args()

    cam = Camera(args.host, args.port, args.connect_timeout)
    try:
        print(cam.run(" ".join(args.command), timeout=args.timeout))
    finally:
        cam.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
