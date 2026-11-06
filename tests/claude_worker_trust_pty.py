"""TRU-225: real Claude 2.1.318, synthetic config/key, no model request.

Rust seeds false, runs this helper, applies claude_task_trust, then runs true.
Both HOME's default file and CLAUDE_CONFIG_DIR are exercised from a worktree.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

repo, config = map(Path, sys.argv[1:3])
trusted = sys.argv[3] == "true"
source = sys.argv[4]
root = config.parent
home = root if source == "home" else root / "decoy-home"
home.mkdir(exist_ok=True)
if source == "config-dir":
    # Opposite trust in HOME proves which file this version actually reads.
    decoy = json.loads(config.read_text())
    for project in decoy["projects"].values():
        project["hasTrustDialogAccepted"] = not trusted
    (home / ".claude.json").write_text(json.dumps(decoy))
    before = (home / ".claude.json").read_bytes()
env = {
    "HOME": str(home), "PATH": os.environ["PATH"], "TERM": "xterm-256color",
    "ANTHROPIC_API_KEY": "gitterm-v6-fixture-not-a-real-key",
    # Never send the synthetic credential to a real inference endpoint.
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:1",
    "XDG_CONFIG_HOME": str(home / ".config"),
    "XDG_CACHE_HOME": str(home / ".cache"),
    "XDG_STATE_HOME": str(home / ".local/state"),
}
if source == "config-dir":
    env["CLAUDE_CONFIG_DIR"] = str(root)
binary = shutil.which("claude")
assert binary, "claude must be installed"
assert subprocess.check_output([binary, "--version"], env=env).strip() == b"2.1.318 (Claude Code)"
pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.execve(binary, [binary], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
output = b""
try:
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        if not select.select([fd], [], [], 0.1)[0]:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        output += chunk
        if b"\x1b[6n" in chunk:
            os.write(fd, b"\x1b[1;1R")
        text = re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", output)
        prompt = b"Yes,Itrustthisfolder" in re.sub(rb"\s+", b"", text)
        composer = b'\xe2\x9d\xaf\xc2\xa0' in text and b'Try "' in text
        if prompt or composer:
            break
finally:
    # Only our own newly forked child; bounded cleanup even if it ignores TERM.
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    os.close(fd)
text = re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", output)
prompt = b"Yes,Itrustthisfolder" in re.sub(rb"\s+", b"", text)
composer = b'\xe2\x9d\xaf\xc2\xa0' in text and b'Try "' in text
assert prompt == (not trusted), f"expected trust={trusted}; screen={text[-2500:]!r}"
assert not trusted or composer, f"trusted launch must reach composer: {text[-2500:]!r}"
if source == "config-dir":
    assert before == (home / ".claude.json").read_bytes(), "decoy HOME config changed"
print(f"Claude 2.1.318 PTY, {source}, canonical main key only, linked worktree: "
      + ("true -> composer, no trust dialog" if trusted else "false -> Yes, I trust this folder"))
