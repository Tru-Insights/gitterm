#!/usr/bin/env python3
# A synthetic protocol peer: it never imports or launches an agent.
import json, sys
from pathlib import Path
import signal
signal.alarm(20)
frames = []
def read():
    value = json.loads(sys.stdin.readline())
    frames.append(value)
    return value
def emit(value):
    print(json.dumps(value), flush=True)
init = read()
assert init['request']['subtype'] == 'initialize'
assert '--resume=sid' in sys.argv
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{'current_permission_mode':'default','models':[]}}})
prompt = read()
assert prompt['type'] == 'user' and prompt['uuid'] == 'prompt-1'
emit({'type':'system','subtype':'init','session_id':'synthetic-chat','model':'synthetic-model'})
emit({'type':'control_request','request_id':'permission-1','request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'tool-1','input':{'command':'true'}}})
answer = read()
assert answer['response']['response']['behavior'] == 'deny'
emit({'type':'control_request','request_id':'question-1','request':{'subtype':'can_use_tool','tool_name':'AskUserQuestion','tool_use_id':'question-tool','input':{'questions':[{'question':'Pick a shape','options':[{'label':'Square','description':'Four sides'}],'multiSelect':False}]}}})
question = read()
assert question['response']['response']['updatedInput']['answers'] == {'Pick a shape':'Square'}
interrupt = read()
assert interrupt['request']['subtype'] == 'interrupt'
emit({'type':'control_response','response':{'subtype':'success','request_id':interrupt['request_id'],'response':{}}})
emit({'type':'result','subtype':'error_during_execution','is_error':True,'errors':['Interrupted by user'],'usage':{}})
assert not sys.stdin.read() # shutdown closes stdin
# Host-generated request ids contain pid/counter: normalize only those ids.
for frame in frames:
    if frame.get('type') == 'control_request':
        frame['request_id'] = '<host-request>'
Path('stdin.json').write_text(json.dumps(frames))

if Path('hang').exists():
    import time
    while True: time.sleep(1)
