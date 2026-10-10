"""SSH: credentials provisioned from camera.conf, then a real login."""

import hashlib
import os
import subprocess
import time

from conftest import RUST, ssh, ssh_args


def test_password_hashed_and_plaintext_removed(camera):
    assert camera.ok(f"grep -q '^root:pbkdf2-sha256:20000:' {RUST}/etc/anyka-sshd.passwd")
    assert camera.ok(f"grep -qx 'ssh_password=' {RUST}/camera.conf")


def test_host_key_generated_on_the_card(camera):
    camera.wait(lambda: camera.ok(f"test -s {RUST}/etc/anyka-sshd.key"), 60, "host key")


def test_wrong_password_rejected(ssh_port):
    assert ssh(ssh_port, "definitely-not-the-password", "true").returncode != 0


def test_login_as_root_on_armv5(ssh_port, password):
    result = ssh(ssh_port, password, "id -u; uname -m")
    assert result.returncode == 0, result.stderr
    uid, arch = result.stdout.split()[:2]
    assert uid == "0"
    assert arch.startswith("armv5")


def test_interactive_session_gets_a_pty(ssh_port, password):
    assert "/dev/pts/" in ssh(ssh_port, password, "tty", tty=True).stdout


def test_remote_exit_status_is_propagated(ssh_port, password):
    assert ssh(ssh_port, password, "exit 7").returncode == 7


def test_upload_to_sd_card_streams_and_server_stays_responsive(camera, ssh_port, password, tmp_path):
    """`ssh cam 'cat > file' < file` without -t: the regression that wedged the server.

    Input used to go through a PTY with a blocking write on the server's only
    thread: the terminal echoed it back, mangled control bytes, never passed
    on EOF, and once the child stopped reading, every connection hung.
    """
    payload = os.urandom(8 * 1024 * 1024)
    source = tmp_path / "payload.bin"
    source.write_bytes(payload)
    remote = f"{RUST}/itest-upload.bin"
    command = f"cat > {remote}.part && mv {remote}.part {remote} && md5sum {remote}"

    env = {**os.environ, "SSHPASS": password}
    with open(source, "rb") as stdin:
        upload = subprocess.Popen(ssh_args(ssh_port, command), env=env, stdin=stdin,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            # Mid-upload, a second login must be served as usual.
            time.sleep(2)
            second = ssh(ssh_port, password, "echo alive")
            assert second.returncode == 0, second.stderr
            assert second.stdout == "alive\n"
            out, err = upload.communicate(timeout=300)
        finally:
            if upload.poll() is None:
                upload.kill()
                upload.wait()
    try:
        assert upload.returncode == 0, err
        # Only md5sum's line: nothing of the input may be echoed back.
        assert out.split() == [hashlib.md5(payload).hexdigest(), remote]
    finally:
        camera.run(f"rm -f {remote} {remote}.part")
