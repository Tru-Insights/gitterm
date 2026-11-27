#!/usr/bin/python3
"""Fake Claude: record OS launch bytes, answer initialize, and exit."""
import json
import os
import sys
from pathlib import Path

guidelines = None
if '--append-system-prompt-file' in sys.argv:
    guidelines = Path(sys.argv[sys.argv.index('--append-system-prompt-file') + 1])
capture = {
    'pid': os.getpid(),
    'argv': [list(os.fsencode(arg)) for arg in sys.argv],
    'environment': {os.fsdecode(key): list(value) for key, value in os.environb.items()},
    'cwd': list(os.getcwdb()),
    'guidelines_path': str(guidelines) if guidelines else None,
    'guidelines': list(guidelines.read_bytes()) if guidelines else [],
}
root = Path(os.environ['GITTERM_LAUNCH_ROOT'])
(root / 'capture.json').write_text(json.dumps(capture))
initialize = json.loads(sys.stdin.readline())
assert initialize['request']['subtype'] == 'initialize'
print(json.dumps({'type': 'control_response', 'response': {
    'subtype': 'success', 'request_id': initialize['request_id'],
    'response': {'hooks_applied': True}}}), flush=True)
