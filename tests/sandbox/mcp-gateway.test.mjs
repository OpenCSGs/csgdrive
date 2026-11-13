import {test} from 'node:test';
import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {Client} from '../../build/sandbox/mcp/node_modules/@modelcontextprotocol/sdk/dist/esm/client/index.js';
import {InMemoryTransport} from '../../build/sandbox/mcp/node_modules/@modelcontextprotocol/sdk/dist/esm/inMemory.js';
import {StdioClientTransport} from '../../build/sandbox/mcp/node_modules/@modelcontextprotocol/sdk/dist/esm/client/stdio.js';
import {createGateway} from '../../build/sandbox/mcp/gateway.mjs';

const fixture=fileURLToPath(new URL('./mcp-fixture.py',import.meta.url));
const provider=(id,prefix='')=>({id,prefix,command:'python3',args:[fixture,id]});
async function setup(t,providers){
 const gateway=await createGateway({cwd:tmpdir(),providers});
 const [left,right]=InMemoryTransport.createLinkedPair();
 const client=new Client({name:'test-agent',version:'1'});
 t.after(async()=>{await gateway.close();await client.close();});
 await gateway.server.connect(left);await client.connect(right);
 return client;
}
test('one connection aggregates paginated tools from three providers and preserves results',async t=>{
 const client=await setup(t,[provider('desktop'),provider('browser','browser_'),provider('files','files_')]);
 const {tools}=await client.listTools();assert.equal(tools.length,7);
 assert.equal(new Set(tools.map(t=>t.name)).size,7);
 for(const [name,id] of [['echo','desktop'],['browser_echo','browser'],['files_last','files']]){
  const result=await client.callTool({name,arguments:{value:'中文😀'}});
  const value=JSON.parse(result.content[0].text);assert.equal(value.provider,id);assert.equal(value.arguments.value,'中文😀');assert.equal(value.name,name.split('_').at(-1));
  assert.equal(result.content[1].data,'dGVzdA==');
 }
 assert.equal((await client.callTool({name:'browser_echo',arguments:{value:'failure'}})).isError,true);
 const status=JSON.parse((await client.callTool({name:'sandbox_tools_status'})).content[0].text);
 assert(status.providers.every(p=>p.connected));
});
test('duplicate tool names are reported and unknown tools are rejected',async t=>{
 const client=await setup(t,[provider('desktop'),provider('browser')]);
 await assert.rejects(()=>client.listTools(),/工具名称重复/);
 const single=await setup(t,[provider('desktop')]);
 await assert.rejects(()=>single.callTool({name:'missing'}),/未知工具/);
});
test('one unavailable provider leaves other tools usable and reports its failure',async t=>{
 const client=await setup(t,[provider('desktop'),{id:'broken',command:'/missing-mcp-server',args:[],prefix:'broken_'}]);
 assert.equal((await client.listTools()).tools.length,3);
 assert(!(await client.callTool({name:'echo',arguments:{value:'works'}})).isError);
 const status=JSON.parse((await client.callTool({name:'sandbox_tools_status'})).content[0].text);
 assert.equal(status.providers[1].connected,false);assert(status.providers[1].error);
});
test('stdio connection negotiates once and disconnect closes its children',async t=>{
 const directory=await mkdtemp(join(tmpdir(),'agentdrive-gateway-'));
 const config=join(directory,'providers.json');await writeFile(config,JSON.stringify({cwd:directory,providers:[provider('desktop'),provider('browser','browser_')]}));
 const transport=new StdioClientTransport({command:process.execPath,args:[fileURLToPath(new URL('../../build/sandbox/mcp/gateway.mjs',import.meta.url)),config],stderr:'pipe'});
 const client=new Client({name:'external-agent',version:'1'});
 t.after(async()=>{await client.close();await rm(directory,{recursive:true,force:true});});
 await client.connect(transport);assert.equal(client.getServerVersion().name,'agentdrive-sandbox');
 assert.equal((await client.listTools()).tools.length,5);
 const pid=transport.pid;await client.close();
 await new Promise(resolve=>setTimeout(resolve,200));
 assert.throws(()=>process.kill(pid,0),/ESRCH/);
});
