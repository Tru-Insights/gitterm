#!/usr/bin/env python3
"""Synthetic Claude protocol child; never contacts a provider."""
import json
import os
import sys
from pathlib import Path

if sys.argv[1:] == ['--help']:
    print('  --restricted\n  --strict-mcp-config\n  --safe-mode\n'
          '  --permission-mode <mode> (choices: "manual", "acceptEdits")')
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status', '--json']:
    print(json.dumps({'fixture_signed_in': True, 'fixture_method': 'fixture_personal',
                      'fixture_plan': 'fixture_pro', 'fixture_root': os.environ['CLAUDE_CONFIG_DIR'],
                      'fixture_policy': None}))
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status']:
    print(json.dumps({'authMethod': 'claude.ai',
                      'configDirectory': os.environ['CLAUDE_CONFIG_DIR']}))
    sys.exit(0)
assert sys.argv[1:] == [
    '--output-format', 'stream-json', '--verbose', '--input-format', 'stream-json',
    '--permission-prompt-tool', 'stdio', '--include-partial-messages', '--no-chrome',
    '--permission-mode', 'default', '--restricted', '--strict-mcp-config', '--safe-mode',
]
init = json.loads(sys.stdin.buffer.readline())
print(json.dumps({'type': 'control_response', 'response': {
    'subtype': 'success', 'request_id': init['request_id'], 'response': {'models': []}
}}), flush=True)
if Path('partial-mode').exists():
    # Consume only the start of a large frame, then die during write_all.
    assert sys.stdin.buffer.read(1)
    sys.exit(9)
# A stale permission response is rejected by the runner, never written here.
assert not sys.stdin.buffer.read()
