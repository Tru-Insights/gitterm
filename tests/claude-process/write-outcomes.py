#!/usr/bin/env python3
import json, signal, sys
signal.alarm(20)
def emit(value): print(json.dumps(value), flush=True)
init = json.loads(sys.stdin.readline())
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}})
# A malformed authoritative input forces question encoding to fail before I/O.
emit({'type':'control_request','request_id':'q1','request':{'subtype':'can_use_tool','tool_name':'AskUserQuestion','tool_use_id':'t1','input':[]}})
answer = json.loads(sys.stdin.readline())
assert answer['response']['request_id'] == 'q1'
assert answer['response']['response']['behavior'] == 'deny'
for expected in ['correlated', 'uncorrelated']:
    prompt = json.loads(sys.stdin.readline())
    assert prompt['message']['content'] == [{'type':'text','text':expected}]
for subtype in ['cancel_async_message', 'interrupt', 'set_permission_mode', 'set_model', 'apply_flag_settings']:
    frame = json.loads(sys.stdin.readline())
    assert frame['request']['subtype'] == subtype
assert not sys.stdin.read()
