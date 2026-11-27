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
        session_id = next((arg.split('=', 1)[1] for arg in sys.argv if arg.startswith('--resume=')), 'synthetic-' + Path.cwd().name)
        project = Path(os.environ['CLAUDE_CONFIG_DIR']) / 'projects' / 'synthetic'
        project.mkdir(parents=True, exist_ok=True)
        with (project / (session_id + '.jsonl')).open('a') as transcript:
            transcript.write(json.dumps({'type':'user','message':{'content':'first turn'}}) + '\n')
            transcript.write(json.dumps({'type':'assistant','message':{'content':[{'type':'text','text':'fake reply'}]}}) + '\n')
        print(json.dumps({'type':'system','subtype':'init','session_id':session_id,'model':'sonnet'}), flush=True)
        print(json.dumps({'type':'stream_event' ,'event':{'type':'content_block_delta','delta':{'type':'text_delta','text':'fake reply'}}}), flush=True)
