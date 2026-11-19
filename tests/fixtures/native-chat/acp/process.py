#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
import signal
signal.alarm(20)
frames = []
Path('pid').write_text(str(os.getpid()))
def read():
    v = json.loads(sys.stdin.readline())
    frames.append(v)
    return v
def emit(v): print(json.dumps(v), flush=True)
def reply(v, body): emit({'jsonrpc':'2.0','id':v['id'],'result':body})
def update(body): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'synthetic-chat','update':body}})
init = read()
assert init['method'] == 'initialize'
reply(init, {'protocolVersion':1,'agentInfo':{'name':'synthetic','version':'0'},'agentCapabilities':{'loadSession':True,'promptCapabilities':{'image':True}}})
start = read()
assert start['method'] in ['session/new','session/load']
if start['method'] == 'session/load':
    assert start['params']['sessionId'] == 'synthetic-chat'
    update({'sessionUpdate':'user_message_chunk','messageId':'history-1','content':{'type':'text','text':'Old synthetic prompt'}})
    update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'Old synthetic reply'}})
reply(start, {'sessionId':'synthetic-chat'})
prompt = read()
assert prompt['method'] == 'session/prompt'
assert prompt['params']['prompt'][0]['type'] == 'image'
emit({'jsonrpc':'2.0','id':'permission-1','method':'session/request_permission','params':{'sessionId':'synthetic-chat','toolCall':{'toolCallId':'tool-1','name':'shell','rawInput':{'command':'true'}},'options':[{'optionId':'once','name':'Allow once','kind':'allow_once'}]}})
answer = read()
assert answer['id'] == 'permission-1' and answer['result']['outcome']['optionId'] == 'once'
cancel = read()
assert cancel['method'] == 'session/cancel'
reply(prompt, {'stopReason':'cancelled'})
# Malformed append-only tail is ignored by the production line reader.
print('{"jsonrpc":', flush=True)
assert not sys.stdin.read()
for frame in frames:
    if frame.get('method') in ['session/new','session/load']:
        frame['params']['cwd'] = '<cwd>'
Path('stdin.json').write_text(json.dumps(frames))

if Path('hang').exists():
    import time
    while True: time.sleep(1)
