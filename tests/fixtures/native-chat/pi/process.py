#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
import signal
signal.alarm(20)
Path('pid').write_text(str(os.getpid()))
args = sys.argv[1:]
assert args[:3] == ['--print','--mode','json']
assert '--session' in args and '--thinking' in args and args[-1] == 'Synthetic prompt'
mode = args[args.index('--model') + 1]
# Model is a scenario selector, normalize only that test input in argv golden.
args[args.index('--model') + 1] = '<scenario>'
Path('argv.json').write_text(json.dumps(args))
if mode in ['stop','cancel','detach']:
    print(json.dumps({'type':'ready','id':'synthetic-pi'}), flush=True)
    if mode == 'detach':
        while not Path('release').exists(): time.sleep(0.01)
        print(json.dumps({'type':'text','text':'Receiver is gone'}), flush=True)
    while True: time.sleep(1)
else:
    print(Path('stream.jsonl').read_text(), flush=True)
    sys.exit(7 if mode == 'exit7' else 0)
