#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
signal.alarm(20)
EXIT_MID_WRITE = False
ANSWER = False
init = json.loads(sys.stdin.readline())
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}}), flush=True)
if ANSWER:
    print(json.dumps({'type':'control_request','request_id':'p1','request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'t1','input':{}}}), flush=True)
Path('prefix').write_bytes(os.read(0, 16))
if EXIT_MID_WRITE:
    os._exit(17)
os.close(0)
while not Path('exit').exists(): time.sleep(0.01)
