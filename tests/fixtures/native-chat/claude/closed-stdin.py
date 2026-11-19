#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
import signal
signal.alarm(20)
Path('pid').write_text(str(os.getpid()))
init = json.loads(sys.stdin.readline())
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}}), flush=True)
json.loads(sys.stdin.readline())
os.close(0)
# Request is emitted after stdin closes, so an answer deterministically fails.
print(json.dumps({'type':'control_request','request_id':'p1','request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'tool-1','input':{'command':'true'}}}), flush=True)
while not Path('exit').exists(): time.sleep(0.01)
