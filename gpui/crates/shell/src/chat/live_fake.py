#!/usr/bin/python3
import json, os, sys
from pathlib import Path
Path('launch.json').write_text(json.dumps({'argv':sys.argv[1:],'env':dict(os.environ)}))
for line in sys.stdin:
    message = json.loads(line)
    if message.get('type') == 'control_request':
        request = message['request']
        response = {'subtype':'success','request_id':message['request_id'],'response':{}}
        if request.get('subtype') == 'initialize':
            response['response'] = {'commands':[], 'models':[], 'permissionMode':'acceptEdits'}
        print(json.dumps({'type':'control_response','response':response}), flush=True)
    elif message.get('type') == 'user':
        assert message['message']['content'] == [{'type':'text','text':'first turn'}]
        print(json.dumps({'type':'stream_event','event':{'type':'content_block_delta','delta':{'type':'text_delta','text':'fake reply'}}}), flush=True)
