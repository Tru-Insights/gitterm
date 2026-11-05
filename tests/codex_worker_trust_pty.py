"""TRU-225 ignored Rust smoke helper; no model prompt or real config writes."""
import fcntl
import hashlib
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tomllib


def screen(command, repo, home):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(CODEX_HOME=str(home), TERM="xterm-256color")
        os.chdir(repo)
        # Exercise GitTerm's shell quoting as well as Codex's TOML parsing.
        os.execv("/bin/sh", ["sh", "-c", "exec " + command + " --no-daemon --no-alt-screen"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
    output = b""
    try:
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                output += chunk
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
                if b"Trust this folder?" in output or b"GPT-6-Luna default" in output:
                    break
    finally:
        # The npm launcher and native CLI share this isolated PTY group.
        os.killpg(pid, signal.SIGTERM)
        until = time.monotonic() + 2
        while os.waitpid(pid, os.WNOHANG) == (0, 0):
            if time.monotonic() >= until:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
                break
            time.sleep(0.05)
        os.close(fd)
    return output


assert subprocess.check_output(["codex", "--version"]).strip() == b"codex-cli 0.164.0"
repo = Path(sys.argv[1])
real_config = Path.home() / ".codex/config.toml"
before = hashlib.sha256(real_config.read_bytes()).digest() if real_config.exists() else None
with tempfile.TemporaryDirectory(prefix="gitterm-v6-tru225-pty-") as directory:
    home = Path(directory)
    config = home / "config.toml"
    config.write_text(
        'model="gpt-6-luna"\ncheck_for_update_on_startup=false\n'
        'approval_policy="on-request"\nsandbox_mode="workspace-write"\n'
        '[projects."/fixture-unrelated"]\ntrust_level="untrusted"\n'
    )
    original = config.read_bytes()
    auth = Path.home() / ".codex/auth.json"
    assert auth.exists(), "Codex auth.json is needed to avoid login onboarding"
    shutil.copyfile(auth, home / "auth.json")
    baseline = screen("codex", repo, home)
    assert b"Trust this folder?" in baseline, "baseline must reach trust prompt"
    trusted = screen(sys.argv[2], repo, home)
    assert b"Trust this folder?" not in trusted, "override must skip trust prompt"
    assert b"GPT-6-Luna default" in trusted, "override must reach composer, not another dialog"
    after = tomllib.loads(config.read_text())
    expected = tomllib.loads(original.decode())
    # Codex 0.164.0 writes these TUI onboarding fields itself. GitTerm's
    # read-only helper is separately byte-tested; trust and permissions must
    # remain exactly as seeded, with no persisted launch-root trust.
    tui = after.pop("tui", {})
    assert set(tui) <= {"screen_reader_detection_done", "model_availability_nux"}
    assert after == expected, "trust overlay must not persist or change permissions"
assert before == (hashlib.sha256(real_config.read_bytes()).digest() if real_config.exists() else None)
print("Codex 0.164.0 PTY: baseline trust prompt, override composer, trust/permissions and real config unchanged")
