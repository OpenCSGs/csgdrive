const {test,before,after}=require('node:test');
const assert=require('node:assert/strict');
const {createServer}=require('node:http');
const {readFile}=require('node:fs/promises');
const path=require('node:path');
const {chromium}=require('playwright');
let server,browser,endpoint;
before(async()=>{
  const directory=path.resolve(__dirname,'../../crates/agentdrive-mcp/dashboard');
  server=createServer(async(req,res)=>{
    const pathname=new URL(req.url,'http://localhost').pathname;
    const name=['/sandboxes','/branches'].includes(pathname)?'index.html':pathname.slice(1);
    if(!/^[a-z.-]+$/.test(name))return res.writeHead(404).end();
    try{const body=await readFile(path.join(directory,name));res.writeHead(200,{'Content-Type':{'.html':'text/html','.js':'text/javascript','.css':'text/css'}[path.extname(name)]||'text/plain'}).end(body);}catch{res.writeHead(404).end();}
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));endpoint=`http://127.0.0.1:${server.address().port}`;
  browser=await chromium.launch({headless:true});
});
after(async()=>{await browser?.close();if(server)await new Promise(resolve=>server.close(resolve));});
async function setup(t,options={}){
  const page=await browser.newPage({viewport:{width:1440,height:1000}}),errors=[],calls=[];
  const data={enabled:true,errors:[],entries:[
    {workspace:'alpha',workspace_name:'项目 Alpha',branch:'main-a',branch_name:'main',can_start:true,sandbox:{state:'ready',desktop_url:'/desktop-a'}},
    {workspace:'beta',workspace_name:'项目 Beta',branch:'branch-b',branch_name:'feature',can_start:true,sandbox:{state:'stopped'}},
    {workspace:'beta',workspace_name:'项目 Beta',branch:'new-b',branch_name:'new-branch',can_start:true,sandbox:null},
  ],template:{max_instances:2,network_slots:2,vcpus:4,memory_gib:8,disk_gib:16,manifest:{name:'Ubuntu Desktop',os:'Ubuntu 22.04',desktop:'GNOME / X11',browser:'Chromium 160',locale:'C.UTF-8',fonts:['Noto CJK','Noto Color Emoji'],tools:['computer-use-linux 0.7.53'],workspace_path:'/workspace',built_at:'2026-11-13T00:00:00Z'}},...options};
  page.on('pageerror',e=>errors.push(e.message));t.after(async()=>{await page.close();assert.deepEqual(errors,[]);});
  await page.route('**/api/**',async route=>{
    const url=new URL(route.request().url()),method=route.request().method();calls.push({path:url.pathname,method});
    if(url.pathname==='/api/workspaces')return route.fulfill({json:data.enabled?[{id:'alpha',name:'项目 Alpha'},{id:'beta',name:'项目 Beta'}]:[]});
    if(url.pathname==='/api/sandboxes')return data.failure?route.fulfill({status:503,json:{error:{message:'服务暂时不可用'}}}):route.fulfill({json:data});
    if(url.pathname.endsWith('/mcp-config'))return route.fulfill({json:{mcpServers:{ubuntu_desktop:{command:'python3',args:[url.pathname]}}}});
    const entry=data.entries.find(e=>url.pathname.includes(`/branches/${e.branch}/sandbox`));
    if(entry && url.pathname.endsWith('/delete')) { entry.sandbox=null; return route.fulfill({json:{deleted:true}}); }
    if(entry){if(method!=='GET')entry.sandbox={state:method==='POST'?'ready':'stopped',desktop_url:'/desktop-b'};return route.fulfill({json:method==='GET'?{enabled:data.enabled,sandbox:entry.sandbox}:entry.sandbox});}
    return route.fulfill({status:404,json:{error:{message:'Not found'}}});
  });
  await page.goto(endpoint+'/sandboxes');await page.waitForFunction(()=>document.querySelector('#updated').textContent.includes('已刷新'));
  return {page,data,calls};
}
test('catalog lists every workspace and controls the selected branch',async t=>{
  const {page,calls}=await setup(t);
  assert.equal(await page.locator('#sandboxes-nav').getAttribute('aria-current'),'page');
  assert.equal(await page.locator('.sandbox-card:visible').count(),2);
  const card=page.locator('[data-branch="branch-b"]');await card.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
  await card.getByRole('link',{name:'打开桌面'}).waitFor();
  await card.getByRole('button',{name:'详情与 MCP'}).click();
  await page.getByText('MCP 配置',{exact:true}).click();
  assert.match(await page.getByRole('textbox',{name:'Sandbox MCP 配置'}).inputValue(),/branches\/branch-b\/sandbox/);
  await page.getByRole('button',{name:'关闭 Sandbox'}).click();
  await card.getByRole('button',{name:'停止 Sandbox'}).click();
  assert.deepEqual(calls.filter(c=>c.method!=='GET'),[{path:'/api/workspaces/beta/branches/branch-b/sandbox',method:'POST'},{path:'/api/workspaces/beta/branches/branch-b/sandbox',method:'DELETE'}]);
});
test('deletion removes the card from existing instances and makes the branch available for creation',async t=>{
  const {page,calls}=await setup(t);
  const card=page.locator('[data-branch="branch-b"]');
  await card.getByRole('button',{name:'删除 Sandbox',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'删除 Sandbox',exact:true});
  assert.match(await dialog.textContent(), /项目 Beta.*feature.*branch-b/);
  await dialog.getByRole('textbox').fill('feature');
  await dialog.getByRole('button',{name:'确认删除'}).click();
  await card.waitFor({state:'hidden'});
  assert.equal(await page.locator('.sandbox-card:visible').count(),1);
  assert.deepEqual(calls.filter(c=>c.method!=='GET'),[{path:'/api/workspaces/beta/branches/branch-b/sandbox/delete',method:'POST'}]);
  await page.getByRole('button',{name:'创建沙盒',exact:true}).click();
  const create=page.getByRole('dialog',{name:'创建沙盒',exact:true});
  await create.getByLabel('工作区',{exact:true}).selectOption('beta');
  await create.getByLabel('分支',{exact:true}).selectOption('branch-b');
  assert(await create.getByRole('button',{name:'创建并启动'}).isEnabled());
});
test('refresh preserves filters and the open dialog; creation selects an unused branch',async t=>{
  const {page}=await setup(t);
  await page.getByRole('combobox',{name:'筛选工作区'}).selectOption('beta');
  await page.getByRole('button',{name:'详情与 MCP'}).filter({visible:true}).click();
  await page.waitForTimeout(2200);assert(await page.getByRole('dialog').isVisible());
  await page.keyboard.press('Escape');assert.equal(await page.getByRole('combobox',{name:'筛选工作区'}).inputValue(),'beta');
  await page.getByRole('button',{name:'创建沙盒',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'创建沙盒',exact:true});
  assert(await dialog.isVisible());
  assert.equal(await dialog.getByLabel('工作区',{exact:true}).inputValue(),'beta');
  assert.equal(await dialog.getByLabel('分支',{exact:true}).inputValue(),'new-b');
  await page.waitForTimeout(2200);
  assert.equal(await dialog.getByLabel('分支',{exact:true}).inputValue(),'new-b');
  await dialog.getByRole('button',{name:'创建并启动',exact:true}).click();
  const progress=page.getByRole('dialog',{name:'Sandbox',exact:true});
  await progress.getByRole('link',{name:'打开桌面'}).waitFor();
  assert.match(await progress.locator('.sandbox-identity').textContent(),/new-branch/);
  await progress.getByRole('button',{name:'关闭 Sandbox'}).click();
  await page.getByRole('combobox',{name:'筛选沙盒状态'}).selectOption('ready');
  await page.getByRole('button',{name:'刷新数据'}).click();
  await page.locator('[data-branch="new-b"]:visible').waitFor();
});
test('creation dialog explains unavailable branches and capacity without sending requests',async t=>{
  const {page,data,calls}=await setup(t);
  await page.getByRole('button',{name:'创建沙盒',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'创建沙盒',exact:true});
  assert(await dialog.getByRole('button',{name:'创建并启动'}).isDisabled());
  assert.match(await dialog.getByRole('status').textContent(),/没有可用分支/);
  assert(await dialog.getByRole('link',{name:'前往分支管理'}).isVisible());
  await dialog.getByLabel('工作区',{exact:true}).selectOption('beta');
  assert.equal(await dialog.getByLabel('分支',{exact:true}).locator('option').count(),1);
  assert(await dialog.getByRole('button',{name:'创建并启动'}).isEnabled());
  data.template.max_instances=1;
  await page.waitForTimeout(2200);
  assert(await dialog.getByRole('button',{name:'创建并启动'}).isDisabled());
  assert.match(await dialog.getByRole('status').textContent(),/已达到 1 个运行实例/);
  await page.keyboard.press('Escape');
  assert(await page.getByRole('button',{name:'创建沙盒',exact:true}).evaluate(e=>e===document.activeElement));
  assert.equal(calls.filter(c=>c.method!=='GET').length,0);
});
test('template shows installed versions, resources, and stays selected across refresh',async t=>{
  const {page}=await setup(t);
  await page.getByRole('tab',{name:'模板内容'}).click();
  await page.getByRole('button',{name:'刷新数据'}).click();
  assert(await page.getByRole('tabpanel',{name:'模板内容'}).isVisible());
  assert.match(await page.locator('#sandbox-template').textContent(),/Chromium 160/);
  assert.match(await page.locator('#sandbox-template').textContent(),/computer-use-linux 0.7.53/);
  assert.match(await page.locator('#sandbox-template').textContent(),/Noto CJK/);
  assert.match(await page.locator('#sandbox-template').textContent(),/4 vCPU · 8 GiB/);
  await page.setViewportSize({width:600,height:900});
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth), JSON.stringify(await page.evaluate(()=>[...document.querySelectorAll('body *')].filter(e=>e.getBoundingClientRect().right>innerWidth+1).map(e=>[e.tagName,e.id,e.className,e.getBoundingClientRect().width]).slice(0,25))));
});
test('unconfigured runtime and failed refresh remain visible',async t=>{
  const {page,data}=await setup(t,{enabled:false,entries:[],template:null});
  assert.equal(await page.locator('#sandboxes-nav').getAttribute('aria-disabled'),'false');
  assert(await page.getByRole('button',{name:'创建沙盒',exact:true}).isDisabled());
  assert.match(await page.locator('#sandbox-instances .sandbox-empty').textContent(),/尚未配置/);
  data.failure=true;await page.getByRole('button',{name:'刷新数据'}).click();
  await page.getByRole('alert').filter({hasText:'沙盒数据刷新失败'}).waitFor();
});
