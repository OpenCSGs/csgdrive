import socket,json,time,base64,argparse,uuid
from pathlib import Path
parser=argparse.ArgumentParser(description='Verify guest desktop input and branch file persistence through MCP')
parser.add_argument('--socket',required=True)
parser.add_argument('--mount',required=True)
parser.add_argument('--artifacts',required=True)
args=parser.parse_args()
artifacts=Path(args.artifacts);artifacts.mkdir(parents=True,exist_ok=True)
mount=Path(args.mount)
filename='desktop-proof-'+uuid.uuid4().hex+'.txt'
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(45);s.connect(args.socket);f=s.makefile('rwb',buffering=0)
f.write(b'CONNECT 7000\n');assert f.readline().startswith(b'OK ')
i=0;report=[]
def rpc(method,params):
 global i
 i+=1;f.write((json.dumps(dict(jsonrpc='2.0',id=i,method=method,params=params))+'\n').encode())
 while True:
  value=json.loads(f.readline())
  if value.get('id')==i:return value
rpc('initialize',dict(protocolVersion='2024-11-05',capabilities={},clientInfo=dict(name='agentdrive-integration-test',version='1')))
f.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
def call(name,args):
 result=rpc('tools/call',dict(name=name,arguments=args))
 assert 'error' not in result,result
 result=result['result'];assert not result.get('isError'),result
 texts=[c['text'] for c in result.get('content',[]) if c['type']=='text'];report.append(dict(tool=name,args=args,result=texts));print(name,str(texts)[:220],flush=True)
 for c in result.get('content',[]):
  if c['type']=='image':(artifacts/'desktop.png').write_bytes(base64.b64decode(c['data']))
 return texts
call('get_app_state',{'include_screenshot':False})
call('setup_accessibility',{})
call('press_key',{'key':'Escape'})
call('press_key',{'key':'alt+F2'})
time.sleep(1)
call('type_text',{'text':'gedit /workspace/'+filename})
call('press_key',{'key':'Return'})
time.sleep(6)
call('list_windows',{})
call('get_app_state',{'app_name_or_bundle_identifier':'gedit','include_screenshot':False})
call('type_text',{'wm_class':'gedit','text':'Created through computer-use-linux inside Ubuntu.\nVerification: '+filename+'\n'})
call('press_key',{'wm_class':'gedit','key':'ctrl+s'})
time.sleep(3)
call('get_app_state',{'app_name_or_bundle_identifier':'gedit','include_screenshot':True})
p=mount/filename
assert p.exists(),'File did not reach the branch mount'
text=p.read_text();assert filename in text,text
(artifacts/'report.json').write_text(json.dumps(dict(mount=str(mount),file=filename,content=text,calls=report),indent=2))
print('VERIFIED BRANCH FILE:',text)
s.close()
