#!/usr/bin/python3
import json, sys
from pathlib import Path
Path('launch.json').write_text(json.dumps({'cwd':str(Path.cwd()), 'argv':sys.argv[1:]}))
for line in sys.stdin:
    message = json.loads(line)
    if message['type'] == 'control_request':
        request = message['request']
        subtype = request['subtype']
        response = {'subtype':'success', 'request_id':message['request_id'], 'response':{}}
        if subtype == 'initialize':
            response['response'] = {'commands':[], 'models':[], 'permissionMode':'default'}
        elif subtype == 'set_model' and request['model'] == 'rejected':
            response = {'subtype':'error', 'request_id':message['request_id'], 'error':'synthetic picker rejection'}
        elif subtype == 'set_permission_mode':
            response['response'] = {'mode':request['mode']}
        elif subtype == 'get_settings':
            response['response'] = {'effective':{'effortLevel':'low'}, 'sources':[{'source':'flagSettings','settings':{'effortLevel':'low'}}], 'applied':{'effort':'low'}}
        print(json.dumps({'type':'control_response', 'response':response}), flush=True)
    elif message['type'] == 'user':
        session_id = next((arg.split('=',1)[1] for arg in sys.argv if arg.startswith('--resume=')), 'fresh-id')
        print(json.dumps({'type':'system', 'subtype':'init', 'session_id':session_id, 'model':'sonnet'}), flush=True)
