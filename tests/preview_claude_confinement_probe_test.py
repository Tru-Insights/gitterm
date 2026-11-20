"""Offline regression tests. Every CLI invocation resolves to our fake Claude."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("probe", ROOT / "scripts/preview_claude_confinement_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
HELP = """Options:
  --restricted  restricted
  --strict-mcp-config  no MCP
  --safe-mode  safe
  --permission-mode <mode>  (choices: "default", "acceptEdits")
  --permission-prompts <target>  (choices: "host", "none")
  --output-format <format>  (choices: "stream-json")
  --verbose  verbose
"""


class ProbeTests(unittest.TestCase):
    def setUp(self):
        # macOS /tmp aliases /private/tmp; the probe deliberately rejects aliases.
        self.temp = tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "host"
        self.home.mkdir()
        self.preview = self.root / "preview"
        self.provider = self.preview / "providers/claude"
        self.provider.mkdir(parents=True)
        self.clone = self.root / "clone"
        (self.clone / ".git/objects/info").mkdir(parents=True)
        (self.clone / ".git/refs/heads").mkdir(parents=True)
        (self.clone / ".git/HEAD").write_text("ref: refs/heads/main\n")
        self.config = self.clone / ".git/config"
        self.config.write_text("[core]\nrepositoryformatversion = 0\n")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.env = {"HOME": str(self.home), "PATH": str(self.bin) + os.pathsep + os.environ["PATH"]}
        self.fake("safe")

    def fake(self, mode, help_text=HELP):
        code = f'''#!{sys.executable}
import json, os, re, sys
from pathlib import Path
MODE = {mode!r}
if sys.argv[1:] == ['--help']:
    print({help_text!r})
    sys.exit(0)
args = sys.argv[1:]
expected = ['--restricted', '--strict-mcp-config', '--safe-mode', '--permission-mode', 'acceptEdits', '-p', '--output-format', 'stream-json', '--verbose', '--permission-prompts', 'none']
assert args[:-1] == expected, args
keys = {{'CLAUDE_CONFIG_DIR','GITTERM_GPUI_CONFIG_DIR','GITTERM_V6_CONFIG_DIR','HOME','XDG_CONFIG_HOME','PATH','TMPDIR','TMP','TEMP'}}
assert set(os.environ) - {{'LC_CTYPE', '__CF_USER_TEXT_ENCODING'}} == keys, set(os.environ)
assert os.environ['PATH'] == '/usr/bin:/bin'
Path('fake-argv.json').write_text(json.dumps(args[:-1]))
Path('fake-env.json').write_text(json.dumps(dict(os.environ)))
if MODE == 'replace-settings-dir' and not Path('.claude').is_symlink():
    external = Path(__file__).parent.parent / 'external-settings'
    external.mkdir()
    (external / 'settings.json').write_text('external-keep')
    Path('.claude').rename('preserved-settings')
    Path('.claude').symlink_to(external, target_is_directory=True)
if MODE == 'exit': sys.exit(7)
if MODE == 'malformed':
    print('not json')
    sys.exit(0)
tools = ['Read', 'Write', 'Edit'] + (['Bash'] if MODE == 'shell' else [])
events = [{{'type':'system','subtype':'init','tools':tools}}]
match = re.search(r'absolute file_path (".*?") with content', args[-1])
if match:
    target = Path(json.loads(match[1]))
    if MODE != 'refuse':
        events.append({{'type':'assistant','message':{{'content':[{{'type':'tool_use','name':'Write','input':{{'file_path':str(target)}}}}]}}}})
    if (target.name.endswith('-allowed.txt') and MODE != 'no-positive') or MODE == 'escape':
        target.write_text('preview-confinement-probe\\n')
events.append({{'type':'result','subtype':'success','is_error':False}})
if MODE == 'no-init': events.pop(0)
if MODE == 'auth-error': events[-1] = {{'type':'result','subtype':'error_during_execution','is_error':True}}
for event in events: print(json.dumps(event))
'''
        fake = self.bin / "claude"
        fake.write_text(code)
        fake.chmod(0o700)

    def run_probe(self, *args):
        return subprocess.run(
            [str(ROOT / "scripts/preview-claude-confinement-probe.sh"), *(args or (str(self.provider), str(self.clone)))],
            env=self.env, text=True, capture_output=True, timeout=20,
        )

    def test_success_uses_exact_flags_env_and_cleans_only_its_fixtures(self):
        keep = self.clone / "keep-me"
        keep.write_text("existing")
        config = self.config.read_bytes()
        output = self.run_probe()
        self.assertEqual(output.returncode, 0, output.stdout + output.stderr)
        self.assertEqual(output.stdout.count("PASS "), 7)
        self.assertNotIn("FAIL ", output.stdout)
        self.assertEqual(self.config.read_bytes(), config)
        self.assertEqual(keep.read_text(), "existing")
        self.assertFalse((self.clone / ".claude").exists())
        self.assertFalse(list(self.clone.glob("preview-confine-*")))
        self.assertFalse((self.home / ".config/gitterm-v6/.probe-sentinel").exists())
        self.assertEqual(json.loads((self.clone / "fake-argv.json").read_text()), probe.arguments("prompt")[:-1])
        child_env = json.loads((self.clone / "fake-env.json").read_text())
        child_env.pop("LC_CTYPE", None)  # CPython may add its UTF-8 locale coercion.
        child_env.pop("__CF_USER_TEXT_ENCODING", None)  # macOS process startup.
        self.assertEqual(child_env, probe.environment(self.preview, self.provider))

    def test_existing_settings_and_config_are_preserved(self):
        settings = self.clone / ".claude/settings.json"
        settings.parent.mkdir()
        settings.write_text('{"existing":true}\n')
        self.assertEqual(self.run_probe().returncode, 0)
        self.assertEqual(settings.read_text(), '{"existing":true}\n')

    def test_escapes_are_found_and_existing_files_never_restored_or_removed(self):
        self.fake("escape")
        # Fake real protected root is entirely inside our synthetic HOME.
        real = self.home / ".config/gitterm-v6/.probe-sentinel"
        real.parent.mkdir(parents=True)
        output = self.run_probe()
        self.assertNotEqual(output.returncode, 0)
        self.assertIn("FAIL absolute real protected root", output.stdout)
        self.assertIn("forbidden path changed", output.stdout)
        self.assertEqual(real.read_text(), probe.MARKER)
        self.assertEqual(self.config.read_text(), probe.MARKER)

    def test_existing_real_sentinel_refuses_without_invoking_chat(self):
        real = self.home / ".config/gitterm-v6/.probe-sentinel"
        real.parent.mkdir(parents=True)
        real.write_text("keep")
        output = self.run_probe()
        self.assertNotEqual(output.returncode, 0)
        self.assertEqual(real.read_text(), "keep")
        self.assertFalse((self.clone / "fake-argv.json").exists())

    def test_cleanup_never_traverses_a_replaced_settings_parent(self):
        self.fake("replace-settings-dir")
        output = self.run_probe()
        self.assertNotEqual(output.returncode, 0)
        self.assertEqual((self.root / "external-settings/settings.json").read_text(), "external-keep")
        self.assertEqual((self.clone / "preserved-settings/settings.json").read_text(), "{}\n")

    def test_refusal_auth_errors_missing_tools_shell_and_missing_positive_fail(self):
        for mode in ("refuse", "exit", "malformed", "auth-error", "no-init", "shell", "no-positive"):
            with self.subTest(mode=mode):
                self.fake(mode)
                output = self.run_probe()
                self.assertNotEqual(output.returncode, 0, output.stdout + output.stderr)
                self.assertIn("FAIL ", output.stdout)
                if mode == "shell":
                    self.assertIn("FAIL shell unavailable", output.stdout)

    def test_missing_restricted_help_refuses_before_chat(self):
        self.fake("safe", HELP.replace("--restricted", "--unrestricted"))
        output = self.run_probe()
        self.assertNotEqual(output.returncode, 0)
        self.assertIn("CLI lacks --restricted", output.stderr)
        self.assertFalse((self.clone / "fake-argv.json").exists())

    def test_independent_clone_checks_and_argument_parsing(self):
        self.assertNotEqual(self.run_probe("one-argument").returncode, 0)
        for redirect in ("commondir", "worktrees", "objects/info/alternates", "objects/info/http-alternates"):
            path = self.clone / ".git" / redirect
            path.write_text(".\n")
            output = self.run_probe()
            self.assertNotEqual(output.returncode, 0, output.stdout)
            path.unlink()
        git = self.clone / ".git"
        git.rename(self.clone / "real-git")
        git.write_text("gitdir: real-git\n")
        self.assertNotEqual(self.run_probe().returncode, 0)
        git.unlink()
        git.symlink_to(self.clone / "real-git", target_is_directory=True)
        self.assertNotEqual(self.run_probe().returncode, 0)

    def test_stream_parser_and_attempt_evidence_fail_closed(self):
        for raw in ("", "{}", "[]", "not-json", '{"type":"assistant","message":{}}'):
            with self.assertRaises(ValueError):
                probe.parse_stream(0, raw)
        with self.assertRaises(ValueError):
            probe.check_help(HELP.replace('"acceptEdits"', '"acceptEditsPlus"'))
        self.assertFalse(probe.attempted([{"name": "Bash", "input": {"file_path": "/path"}}], Path("/path")))


if __name__ == "__main__":
    unittest.main()
