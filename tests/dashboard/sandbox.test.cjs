const {test, before, after} = require('node:test');
const assert = require('node:assert/strict');
const {createServer} = require('node:http');
const {readFile} = require('node:fs/promises');
const path = require('node:path');
const {chromium} = require('playwright');
let server, browser, endpoint;
before(async () => {
  const source = await readFile(path.join(__dirname, '../../crates/agentdrive-mcp/dashboard/sandbox.js'));
  server = createServer((req,res) => req.url === '/sandbox.js'
    ? res.writeHead(200,{'Content-Type':'text/javascript'}).end(source)
    : res.writeHead(200,{'Content-Type':'text/html'}).end('<main></main>'));
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  endpoint = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({headless:true});
});
after(async()=>{ await browser?.close(); if(server) await new Promise(resolve=>server.close(resolve)); });
async function setup(t, enabled = true, desktop = '/desktop') {
  const page=await browser.newPage();t.after(()=>page.close());await page.goto(endpoint);
  await page.evaluate(async ({enabled,desktop}) => {
    const {createSandboxControl}=await import('/sandbox.js');
    window.calls=[];
    let sandbox = null;
    const api=async(url,options={})=>{
      window.calls.push({url,method:options.method||'GET',...(options.body?{body:JSON.parse(options.body)}:{})});
      if (url.endsWith('/proxy')) {
        if (!options.method && window.holdProxy) await new Promise(resolve => { window.finishProxy = resolve; });
        return {url:window.proxyUrl || null};
      }
      if (url.endsWith('/delete')) {
        if (window.failDelete) throw new Error('系统磁盘暂时无法删除');
        if (window.holdDelete) await new Promise(resolve => { window.finishDelete = resolve; });
        sandbox = null; return {deleted:true};
      }
      if (url.endsWith('/force-stop')) { sandbox={...sandbox,state:'stopped',message:'已强制停止'}; return sandbox; }
      if (url.endsWith('/mcp-config')) return {mcpServers:{ubuntu_desktop:{command:'python3',args:['/proxy.py','--socket','/branch-a/vsock.sock']}}};
      if (options.method) {
        if (options.method === 'DELETE' && window.failStop) throw new Error('正常停止失败，请使用强制停止');
        sandbox={state:options.method==='POST'?'ready':'stopped',desktop_url:desktop}; return sandbox;
      }
      return {enabled,sandbox};
    };
    const element=(tag,className='',text='')=>{const e=document.createElement(tag);e.className=className;e.textContent=text;return e;};
    document.querySelector('main').append(createSandboxControl({api,element,workspace:'workspace-a',branch:'branch-a'}));
  },{enabled,desktop});
  await page.getByRole('button',{name:'Sandbox',exact:true}).click();
  return page;
}
test('Sandbox creation targets its branch and exposes desktop and stop actions',async t=>{
  const page=await setup(t);
  await page.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
  assert(await page.getByRole('link',{name:'打开桌面'}).isVisible());
  await page.getByRole('button',{name:'停止 Sandbox'}).click();
  assert(await page.getByRole('button',{name:'启动 Sandbox'}).isVisible());
  const calls=(await page.evaluate(()=>window.calls)).filter(c=>c.url !== '/api/sandboxes');
  assert.deepEqual(calls.map(c=>c.method),['GET','GET','POST','DELETE']);
  assert(calls.every(c=>c.url==='/api/workspaces/workspace-a/branches/branch-a/sandbox'));
});

test('Deletion requires stopped state and exact confirmation, supports cancellation and retry', async t => {
  const page = await setup(t);
  await page.getByRole('button', {name:'启动 Sandbox', exact:true}).click();
  assert(await page.getByRole('button', {name:'删除 Sandbox', exact:true}).isDisabled());
  await page.getByRole('button', {name:'停止 Sandbox'}).click();
  await page.getByRole('button', {name:'删除 Sandbox', exact:true}).click();
  const modal = page.getByRole('dialog', {name:'删除 Sandbox', exact:true});
  assert.match(await modal.textContent(), /workspace.*Downloads/);
  const confirm = modal.getByRole('button', {name:'确认删除'});
  assert(await confirm.isDisabled());
  await modal.getByRole('textbox').fill('wrong-branch'); assert(await confirm.isDisabled());
  await modal.getByRole('textbox').fill('branch-a');
  await modal.getByRole('button', {name:'取消'}).click();
  await page.getByRole('button', {name:'删除 Sandbox', exact:true}).click();
  await page.keyboard.press('Escape');
  assert.equal((await page.evaluate(() => window.calls)).filter(c => c.url.endsWith('/delete')).length, 0);
  await page.getByRole('button', {name:'删除 Sandbox', exact:true}).click();
  await modal.getByRole('textbox').fill('branch-a');
  await page.evaluate(() => { window.failDelete = true; });
  await confirm.click();
  assert.match(await modal.getByRole('alert').textContent(), /暂时无法删除/);
  await page.evaluate(() => { window.failDelete = false; window.holdDelete = true; });
  await confirm.click();
  assert(await modal.getByRole('button', {name:'正在删除…'}).isDisabled());
  await page.keyboard.press('Escape'); assert(await modal.isVisible());
  await page.evaluate(() => window.finishDelete());
  await modal.waitFor({state:'hidden'});
  const deleted = (await page.evaluate(() => window.calls)).filter(c => c.url.endsWith('/delete'));
  assert.equal(deleted.length, 2);
  assert.deepEqual(deleted[1], {url:'/api/workspaces/workspace-a/branches/branch-a/sandbox/delete',method:'POST',body:{confirm_branch:'branch-a'}});
  await page.getByRole('button', {name:'Sandbox', exact:true}).click();
  assert(!(await page.getByRole('button', {name:'删除 Sandbox', exact:true}).isVisible()));
  await page.getByRole('button', {name:'启动 Sandbox', exact:true}).click();
  assert(await page.getByRole('link', {name:'打开桌面'}).isVisible());
});
test('Unavailable runtime disables creation',async t=>{
  const page=await setup(t,false);
  assert(await page.getByRole('button',{name:'启动 Sandbox'}).isDisabled());
  assert.match(await page.getByRole('status').textContent(),/尚未配置/);
});

test('Failed normal stop offers explicit force confirmation and preserves restart', async t => {
  const page = await setup(t);
  await page.getByRole('button', {name:'启动 Sandbox', exact:true}).click();
  await page.evaluate(() => { window.failStop = true; });
  await page.getByRole('button', {name:'停止 Sandbox', exact:true}).click();
  await page.getByRole('button', {name:'强制停止', exact:true}).click();
  const modal = page.getByRole('dialog', {name:'强制停止 Sandbox', exact:true});
  const confirm = modal.getByRole('button', {name:'确认强制停止', exact:true});
  assert(await confirm.isDisabled());
  await modal.getByRole('textbox').fill('branch-a');
  await confirm.click();
  assert(await page.getByRole('button', {name:'启动 Sandbox', exact:true}).isVisible());
  assert(await page.getByRole('button', {name:'删除 Sandbox', exact:true}).isEnabled());
  const call = await page.evaluate(() => window.calls.find(item => item.url.endsWith('/force-stop')));
  assert.deepEqual(call.body, {confirm_branch:'branch-a'});
  assert.equal(call.method, 'POST');
});
test('Desktop links must use the AgentDrive origin',async t=>{
  const page=await setup(t,true,'https://example.org/desktop');
  await page.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
  assert(!(await page.getByRole('link',{name:'打开桌面'}).isVisible()));
  assert.match(await page.getByRole('status').textContent(),/地址无效/);
});

test('Startup polling displays the active step and retains failures', async t => {
  const page = await browser.newPage(); t.after(() => page.close()); await page.goto(endpoint);
  await page.evaluate(async () => {
    const {createSandboxControl} = await import('/sandbox.js');
    window.startupState = {enabled:true,sandbox:null};
    const api = async (url, options={}) => options.method === 'POST'
      ? new Promise(resolve => { window.finishStartup = resolve; })
      : window.startupState;
    const element = (tag, className='', text='') => { const e=document.createElement(tag);e.className=className;e.textContent=text;return e; };
    document.querySelector('main').append(createSandboxControl({api,element,workspace:'w',branch:'b'}));
  });
  await page.getByRole('button',{name:'Sandbox',exact:true}).click();
  await page.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
  await page.evaluate(() => { window.startupState.sandbox={state:'starting',startup_step:'wait_desktop'}; });
  await page.waitForSelector('[data-step="wait_desktop"][data-state="active"]');
  assert.equal(await page.locator('[data-state="complete"]').count(),3);
  assert.equal(await page.locator('.sandbox-step').count(),4);
  assert.equal(await page.locator('[data-step="verify_tools"]').count(),0);
  await page.evaluate(() => {
    const failure={state:'error',startup_step:'wait_desktop',message:'Ubuntu 启动失败'};
    window.startupState.sandbox=failure; window.finishStartup(failure);
  });
  await page.waitForSelector('[data-step="wait_desktop"][data-state="error"]');
  assert(await page.getByRole('button',{name:'重试启动'}).isVisible());
  assert.match(await page.getByRole('status').textContent(),/Ubuntu 启动失败/);
});


test('Dialog keeps branch rows compact and restores focus after Escape',async t=>{
  const page=await setup(t);
  assert(await page.getByRole('dialog').isVisible());
  assert.equal(await page.locator('main .sandbox-controls > button').count(),1);
  assert.equal(await page.locator('main .sandbox-steps').count(),0);
  await page.keyboard.press('Escape');
  assert.equal(await page.getByRole('dialog').count(),0);
  assert(await page.getByRole('button',{name:'Sandbox',exact:true}).evaluate(e=>e===document.activeElement));
  await page.getByRole('button',{name:'Sandbox',exact:true}).click();
  await page.evaluate(()=>{
    const control=document.querySelector('.sandbox-controls');
    document.querySelector('main').replaceChildren();
    document.querySelector('main').append(control);
  });
  assert(await page.getByRole('dialog').isVisible());
  await page.getByRole('button',{name:'关闭 Sandbox'}).click();
  assert.equal(await page.getByRole('dialog').count(),0);
});

test('MCP configuration is fetched for the branch and can be downloaded',async t=>{
  const page=await setup(t);
  await page.getByText('MCP 配置',{exact:true}).click();
  const config=page.getByRole('textbox',{name:'Sandbox MCP 配置'});
  await config.waitFor();
  assert.equal(JSON.parse(await config.inputValue()).mcpServers.ubuntu_desktop.args[2],'/branch-a/vsock.sock');
  const download=page.waitForEvent('download');
  await page.getByRole('button',{name:'下载 JSON'}).click();
  assert.equal((await download).suggestedFilename(),'sandbox-mcp-branch-a.json');
  assert((await page.evaluate(()=>window.calls)).some(c=>c.url==='/api/workspaces/workspace-a/branches/branch-a/sandbox/mcp-config'));
});

test('Dialog reopens a running Sandbox and disposes its overlay',async t=>{
  const page=await setup(t);
  await page.evaluate(()=>{
    const control=document.querySelector('.sandbox-controls');
    window.control=control;
  });
  await page.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
  await page.getByRole('button',{name:'关闭 Sandbox'}).click();
  await page.getByRole('button',{name:'Sandbox',exact:true}).click();
  assert(await page.getByRole('link',{name:'打开桌面'}).isVisible());
  await page.evaluate(()=>window.control.dispose());
  assert.equal(await page.locator('dialog').count(),0);
});

test('assistant opens the full CSGClaw UI and proxy input waits for saved settings', async t => {
  const page = await setup(t);
  await page.evaluate(() => {
    window.holdProxy = true;
    document.querySelector('.sandbox-controls').update({state:'ready',template_id:'ubuntu-csgclaw',services:[{id:'csgclaw',label:'打开助手'}]}, true);
  });
  const link = page.getByRole('link', {name:'打开助手',exact:true});
  const url = new URL(await link.getAttribute('href'));
  assert.equal(url.hostname, 'csgclaw.workspace-a.branch-a.localhost');
  assert.equal(url.hash, '#/new?agent=agent-agentdrive');
  await page.getByText('代理配置',{exact:true}).click();
  const input = page.getByRole('textbox',{name:'HTTP 代理地址'});
  assert(await input.isDisabled());
  assert(await page.getByRole('button',{name:'保存代理配置'}).isDisabled());
  await page.waitForFunction(() => !!window.finishProxy);
  await page.evaluate(() => window.finishProxy());
  await input.fill('http://127.0.0.1:7890');
  await page.getByRole('button',{name:'保存代理配置'}).click();
  await page.getByText('已保存，下次启动沙盒时生效。',{exact:true}).waitFor();
  const saved = (await page.evaluate(() => window.calls)).find(call => call.method === 'PUT');
  assert.deepEqual(saved.body, {url:'http://127.0.0.1:7890'});
});
