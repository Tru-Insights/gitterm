#!/usr/bin/python3
import json, os, sys
from pathlib import Path
root = Path(os.getcwd())
with (root / 'launches').open('a') as out:
    out.write('launch\n')
(root / 'child.json').write_text(json.dumps({'argv': sys.argv[1:], 'env': dict(os.environ), 'cwd': str(root)}))
def emit(frame):
    print(json.dumps(frame), flush=True)
for line in sys.stdin:
    f = json.loads(line)
    with (root / 'wire.jsonl').open('a') as out:
        out.write(json.dumps(f) + '\n')
    method, p = f.get('method'), f.get('params', {})
    result = {}
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {'loadSession': not (root / 'no-load').exists(), 'mcpCapabilities': {'http': True}, '_meta': {'antigravity': {'systemPrompt': {'append': True}}}}}
    elif method in ['session/new', 'session/load']:
        if method == 'session/load' and (root / 'fail-load').exists():
            emit({'jsonrpc':'2.0', 'id':f['id'], 'error':{'code':-32000, 'message':'Placeholder load failure'}})
            continue
        sid = p.get('sessionId', 'placeholder-new-session')
        if method == 'session/load':
            for kind, text in [('user_message_chunk', 'placeholder saved prompt'), ('agent_message_chunk', 'placeholder saved reply')]:
                emit({'jsonrpc':'2.0', 'method':'session/update', 'params':{'sessionId':sid, 'update':{'sessionUpdate':kind, 'content':{'type':'text', 'text':text}}}})
        result = {'sessionId': sid, 'models': {'currentModelId':'placeholder-model', 'availableModels':[{'modelId':'placeholder-model','name':'Placeholder'}]}, 'modes': {'currentModeId':'default', 'availableModes':[{'id':'default','name':'Default'}, {'id':'plan','name':'Plan'}]}}
    elif method == 'session/set_model' and p.get('modelId') == 'placeholder-retired-model':
        emit({'jsonrpc':'2.0','id':f['id'],'error':{'code':-32602,'message':'Placeholder retired model'}})
        continue
    elif method == 'session/prompt':
        if (root / 'permission').exists():
            emit({'jsonrpc':'2.0','id':900,'method':'session/request_permission','params':{'sessionId':p['sessionId'],'toolCall':{'toolCallId':'placeholder-tool','title':'Placeholder tool','kind':'execute','status':'pending'},'options':[{'optionId':'placeholder-once','name':'Allow once','kind':'allow_once'},{'optionId':'placeholder-always','name':'Always allow','kind':'allow_always'},{'optionId':'placeholder-deny','name':'Deny','kind':'reject_once'}]}})
            answer = json.loads(sys.stdin.readline())
            with (root / 'wire.jsonl').open('a') as out:
                out.write(json.dumps(answer) + '\n')
            assert answer['result']['outcome']['optionId'] == 'placeholder-always'

        emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':p['sessionId'], 'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'placeholder continued reply'}}}})
        result = {'stopReason':'end_turn'}
    if 'id' in f:
        emit({'jsonrpc':'2.0','id':f['id'],'result':result})
        if method == 'session/prompt' and (root / 'exit-after-prompt').exists():
            break
