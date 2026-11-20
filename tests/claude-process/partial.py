#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
signal.alarm(20)
Path('pid').write_text(str(os.getpid()))
init = json.loads(sys.stdin.readline())
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}}), flush=True)
prompt = json.loads(sys.stdin.readline())
assert prompt['type'] == 'user'
line = json.dumps({'type':'stream_event','event':{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'partial output'}}})
sys.stdout.write(line[:25]); sys.stdout.flush()
time.sleep(0.05)
sys.stdout.write(line[25:] + '\n'); sys.stdout.flush()
sys.stdout.write(json.dumps({'type':'stream_event','event':{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':' final fragment'}}})); sys.stdout.flush()
sys.exit(17)
