"""Check live turns against the real chat page in an isolated Chrome.

Run: python3 tests/check_live_turns.py --chrome <path-to-cached-chrome-headless-shell>
Optional: --screenshot <output.png>
"""

import argparse
from html.parser import HTMLParser
from pathlib import Path
import subprocess
import tempfile


class ResultParser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.in_result = False
        self.result = ""

    def handle_starttag(self, tag, attrs):
        if tag == "pre" and ("id", "live-turn-test-result") in attrs:
            self.in_result = True

    def handle_endtag(self, tag):
        if tag == "pre":
            self.in_result = False

    def handle_data(self, data):
        if self.in_result:
            self.result += data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chrome", required=True)
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    page = (root / "assets/agent_chat.html").read_text()
    vendor = "\n;\n".join(
        (root / "assets/vendor" / name).read_text()
        for name in ["marked.min.js", "highlight.min.js"]
    )
    page = page.replace("/*__VENDOR_JS__*/", vendor, 1).replace("/*__BAR_HINT__*/", "", 1)
    tests = (root / "tests/fixtures/live_turns.js").read_text()
    page = page.replace("</body>", "<script>" + tests + "</script></body>")
    with tempfile.TemporaryDirectory(prefix="gitterm-v6-live-turns-") as temp:
        temp = Path(temp)
        fixture = temp / "live-turns.html"
        fixture.write_text(page)
        command = [
            args.chrome, "--headless", "--no-first-run", "--no-default-browser-check",
            f"--user-data-dir={temp / 'profile'}", "--window-size=1100,1000",
            "--virtual-time-budget=1000", "--dump-dom",
        ]
        if args.screenshot:
            command.append(f"--screenshot={args.screenshot.resolve()}")
        result = subprocess.run(command + [fixture.as_uri()], capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(f"Chrome exited {result.returncode}: {result.stderr}")
        parsed = ResultParser()
        parsed.feed(result.stdout)
        if not parsed.result:
            raise RuntimeError("Live turn test did not finish. Chrome stderr: " + result.stderr)
        print(parsed.result)
        if "FAIL" in parsed.result:
            raise SystemExit(1)


if __name__ == "__main__":
    main()
