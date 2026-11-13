import {readFile} from 'node:fs/promises';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {pathToFileURL} from 'node:url';
import {Client} from '@modelcontextprotocol/sdk/client/index.js';
import {StreamableHTTPClientTransport} from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import {StdioClientTransport} from '@modelcontextprotocol/sdk/client/stdio.js';
import {Server} from '@modelcontextprotocol/sdk/server/index.js';
import {StdioServerTransport} from '@modelcontextprotocol/sdk/server/stdio.js';
import {CallToolRequestSchema, ListToolsRequestSchema, ListRootsRequestSchema, ToolListChangedNotificationSchema, McpError, ErrorCode} from '@modelcontextprotocol/sdk/types.js';

const execute = promisify(execFile);
const instructions = '这是 AgentDrive Sandbox 的统一 MCP 入口。browser_* 工具通过 Playwright 操作 VNC 中可见的 Chromium，桌面工具操作同一个 Ubuntu 桌面。网页导航、标签页、按钮和表单优先使用 browser_*；终端和系统窗口使用桌面工具。下载目录为 /workspace/Downloads，文件属于当前 AgentDrive 分支。调用 browser_tabs 可以查看用户已打开的标签页，操作前确认目标标签页。关闭 MCP 连接会保留浏览器和登录状态。请勿并行操作同一个桌面。sandbox_tools_status 可以检查内部工具服务。';
const statusTool = {name:'sandbox_tools_status',description:'查看统一 MCP 入口的内部服务状态和工具数量。',inputSchema:{type:'object',properties:{},additionalProperties:false},annotations:{readOnlyHint:true}};

export async function createGateway(config) {
  if (!Array.isArray(config.providers) || !config.providers.length) throw new Error('MCP providers 配置不能为空');
  const identifiers = new Set();
  const providers = config.providers.map(value => {
    if (!/^[a-z][a-z0-9_-]*$/.test(value.id) || identifiers.has(value.id) || (!value.url && (typeof value.command !== 'string' || !Array.isArray(value.args) || !value.args.every(arg => typeof arg === 'string'))) || !/^[a-zA-Z0-9_-]*$/.test(value.prefix ?? '')) throw new Error('MCP provider 配置无效或名称重复');
    identifiers.add(value.id);
    return {config:value, client:null, transport:null, tools:[], error:null, connecting:null};
  });
  const server = new Server({name:'agentdrive-sandbox',version:'1.0.0'}, {capabilities:{tools:{listChanged:true}},instructions});
  let closing = false, initialized = false, routes = new Map(), queue = Promise.resolve();
  const pendingRequests = new Set();
  function track(promise) { pendingRequests.add(promise); promise.then(() => pendingRequests.delete(promise), () => pendingRequests.delete(promise)); return promise; }
  server.oninitialized = () => { initialized = true; };
  async function connect(provider) {
    if (closing) throw new Error('MCP 连接正在关闭');
    if (provider.client) return;
    if (provider.connecting) return provider.connecting;
    provider.connecting = (async () => {
      const client = new Client({name:'agentdrive-gateway',version:'1.0.0'}, {capabilities:{roots:{listChanged:false}}});
      client.setRequestHandler(ListRootsRequestSchema, async () => ({roots:[{uri:'file:///workspace',name:'当前 AgentDrive 分支'}]}));
      client.setNotificationHandler(ToolListChangedNotificationSchema, async () => {
        if (initialized && !closing) await server.sendToolListChanged();
      });
      const transport = provider.config.url ? new StreamableHTTPClientTransport(new URL(provider.config.url)) : new StdioClientTransport({command:provider.config.command,args:provider.config.args,cwd:config.cwd || '/workspace',env:{...process.env,...provider.config.env},stderr:'inherit'});
      provider.transport = transport;
      client.onclose = () => {
        if (provider.client === client) { provider.client = null; provider.error = '内部 MCP 服务已断开，下次调用时重新连接'; }
      };
      try {
        await client.connect(transport, {timeout:15000});
        if (closing) { await client.close(); return; }
        provider.client = client; provider.error = null;
      } catch (error) {
        provider.error = error.message; await transport.close(); throw error;
      }
    })();
    try { await provider.connecting; } finally { provider.connecting = null; }
  }
  async function catalog() {
    await Promise.all(providers.map(async provider => {
      try {
        await connect(provider);
        const tools = [], cursors = new Set(); let cursor;
        do {
          const result = await provider.client.listTools(cursor ? {cursor} : {}, {timeout:15000});
          tools.push(...result.tools); cursor = result.nextCursor;
          if (cursor && cursors.has(cursor)) throw new Error('内部 MCP 服务返回重复分页标记');
          cursors.add(cursor);
        } while (cursor);
        provider.tools = tools; provider.error = null;
      } catch (error) { provider.error = error.message; }
    }));
    const next = new Map(), tools = [statusTool];
    for (const provider of providers) for (const tool of provider.tools) {
      const name = (provider.config.prefix || '') + tool.name;
      if (name === statusTool.name || next.has(name)) throw new McpError(ErrorCode.InternalError, `工具名称重复：${name}，请在 providers.json 中配置 prefix`);
      if (!/^[a-zA-Z0-9_.-]{1,128}$/.test(name)) throw new Error(`工具名称无效：${name}`);
      next.set(name, {provider,original:tool.name}); tools.push({...tool,name});
    }
    routes = next; return {tools};
  }
  server.setRequestHandler(ListToolsRequestSchema, () => track(catalog()));
  server.setRequestHandler(CallToolRequestSchema, (request, extra) => {
    const invoke = async () => {
      if (request.params.name === statusTool.name) {
        if (!routes.size) await catalog();
        return {content:[{type:'text',text:JSON.stringify({browser:`Chromium / DISPLAY=${process.env.DISPLAY || ':0'}`,downloads:'/workspace/Downloads',providers:providers.map(provider => ({id:provider.config.id,connected:!!provider.client,tools:provider.tools.length,error:provider.error}))})}]};
      }
      if (!routes.has(request.params.name)) await catalog();
      const route = routes.get(request.params.name);
      if (!route) throw new McpError(ErrorCode.InvalidParams, `未知工具：${request.params.name}`);
      try {
        extra.signal.throwIfAborted();
        await connect(route.provider);
        const prepare = route.provider.config.prepare;
        if (prepare) await execute(prepare[0],prepare.slice(1), {timeout:20000,signal:extra.signal});
        return await route.provider.client.callTool({...request.params,name:route.original}, undefined, {timeout:120000,signal:extra.signal,
          onprogress: request.params._meta?.progressToken === undefined ? undefined : progress => server.notification({method:'notifications/progress',params:{...progress,progressToken:request.params._meta.progressToken}})});
      } catch (error) {
        return {isError:true,content:[{type:'text',text:`${route.provider.config.id}：${error.message}`}]};
      }
    };
    const pending = queue.then(invoke, invoke); queue = pending.catch(() => {}); return track(pending);
  });
  async function close() {
    if (closing) return; closing = true;
    await Promise.allSettled(providers.map(async provider => {
      await provider.connecting?.catch(() => {});
      await (provider.client ? provider.client.close() : provider.transport?.close());
    }));
    await server.close();
  }
  return {server,close,drain:() => Promise.allSettled([...pendingRequests])};
}

async function main() {
  const config = JSON.parse(await readFile(process.argv[2] || new URL('./providers.json',import.meta.url),'utf8'));
  const gateway = await createGateway(config);
  const shutdown = () => gateway.close().catch(error => console.error(error.message));
  process.stdin.once('end', () => gateway.drain().then(shutdown));
  process.on('SIGTERM', shutdown); process.on('SIGINT', shutdown);
  process.stdout.on('error', shutdown);
  await gateway.server.connect(new StdioServerTransport());
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(error => { console.error(error.message); process.exitCode = 1; });
