const {test, before, after} = require('node:test');
const assert = require('node:assert/strict');
const {createServer} = require('node:http');
const {readFile} = require('node:fs/promises');
const path = require('node:path');
const {chromium} = require('playwright');
let server, browser, endpoint;
before(async () => {
  const directory = path.resolve(__dirname, '../../crates/agentdrive-mcp/dashboard');
  server = createServer(async (request, response) => {
    const pathname = new URL(request.url, 'http://localhost').pathname;
    const name = ['/', '/archive', '/dashboard', '/mounts', '/branches', '/settings'].includes(pathname) ? 'index.html' : pathname.slice(1);
    if (!/^[a-z.-]+$/.test(name)) return response.writeHead(404).end();
    try {
      const body = await readFile(path.join(directory, name));
      response.writeHead(200, {'Content-Type':{'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml'}[path.extname(name)]}).end(body);
    } catch { response.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  endpoint = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({headless:true});
});
after(async () => { await browser?.close(); if (server) await new Promise(resolve => server.close(resolve)); });
async function setup(t, options = {}) {
  const page = await browser.newPage({viewport:{width:1440,height:900}});
  const errors = [], requests = [];
  const data = {workspaces:[{id:'first',name:'项目 Alpha'},{id:'second',name:'项目 Beta'}], ...options};
  page.on('pageerror', error => errors.push(error.message));
  t.after(async () => { await page.close(); assert.deepEqual(errors, []); });
  await page.route('**/api/**', async route => {
    const request = route.request(), url = new URL(request.url());
    requests.push(url);
    assert.equal(request.headers().authorization, undefined);
    if (url.pathname === '/api/settings') return route.fulfill({json:{revision:'config',config_path:'/config',runtime:{cache:{max_bytes:2147483648},allowed_origins:[],remote:null},startup:{listen:'127.0.0.1:7421',data_dir:'/state',mount_roots:[],directory_roots:[]}}});
    if (url.pathname === '/api/workspaces') return data.listError ? route.fulfill({status:503,json:{error:{message:'Workspaces unavailable'}}}) : route.fulfill({json:data.workspaces});
    if (url.pathname === '/api/commands/workspace_delete') {
      const input = request.postDataJSON();
      (data.deletions ||= []).push(input);
      if (data.deleteError) return route.fulfill({json:{error:{message:data.deleteError},local_saved:false}});
      data.workspaces = data.workspaces.filter(w => w.id !== input.request.workspace);
      return route.fulfill({json:{phase:'complete',local_saved:true,error:null}});
    }
    const workspace = data.workspaces.find(workspace => url.pathname.split('/')[3] === workspace.id);
    if (!workspace) return route.fulfill({status:404,json:{error:{message:'Not found'}}});
    if (data.pending && url.pathname.endsWith('/archive')) await data.pending(workspace.id);
    const status = {workspace,branches:[],mounts:[],capabilities:{},location:'local',pending_jobs:0};
    if (url.pathname.endsWith('/archive')) return route.fulfill({json:status});
    if (url.pathname.endsWith('/relationships')) return route.fulfill({json:{entries:[],total:0,next:null}});
    return route.fulfill({json:{status,branches:[],revisions:[],origins:[],summary:{branch_count:0,mount_count:0,ready_mount_count:0,mounted_branch_count:0}}});
  });
  return {page,data,requests};
}
async function settled(page) { await page.waitForFunction(() => document.querySelector('#updated').textContent.includes('已刷新') && !document.querySelector('#refresh').disabled); }

test('root opens the first workspace archive without sign-in or remembered selection', async t => {
  const {page,requests} = await setup(t);
  await page.addInitScript(() => { sessionStorage.setItem('agentdrive-workspace','second'); sessionStorage.setItem('agentdrive-token','obsolete'); });
  await page.goto(endpoint+'/#token=obsolete'); await settled(page);
  assert.equal(new URL(page.url()).pathname,'/archive');
  assert.equal(new URL(page.url()).searchParams.get('workspace'),'first');
  assert.equal(new URL(page.url()).hash,'');
  assert.equal(await page.locator('#login').count(),0);
  assert.equal(await page.evaluate(() => sessionStorage.getItem('agentdrive-token')),null);
  assert.equal(await page.locator('#archive-nav').getAttribute('aria-current'),'page');
  assert.equal(await page.locator('[data-workspace=first]').getAttribute('aria-current'),'true');
  assert(requests.some(url => url.pathname === '/api/workspaces/first/archive'));
});

test('workspace selection resets the view and browser history restores workspace and page', async t => {
  const {page} = await setup(t);
  await page.goto(endpoint+'/dashboard?workspace=first&branch=old&view=merges&merge=old'); await settled(page);
  await page.locator('[data-workspace=second]').click(); await settled(page);
  assert.equal(new URL(page.url()).pathname,'/archive');
  assert.equal(new URL(page.url()).search,'?workspace=second');
  await page.locator('#branches-nav').click(); await settled(page);
  assert.equal(await page.locator('#branches-page').isVisible(),true);
  assert.equal(await page.locator('#branches-nav').getAttribute('aria-current'),'page');
  await page.goBack(); await settled(page);
  assert.equal(new URL(page.url()).pathname,'/archive');
  await page.goBack(); await settled(page);
  assert.equal(new URL(page.url()).searchParams.get('workspace'),'first');
  assert.equal(await page.locator('#history-nav').getAttribute('aria-current'),'page');
});

test('workspace search, collapse and refresh preserve navigation state', async t => {
  const {page,data} = await setup(t);
  data.workspaces.push(...Array.from({length:40},(_,i)=>({id:`extra-${i}`,name:`项目 ${i}`})));
  await page.goto(endpoint+'/'); await settled(page);
  await page.locator('#workspace-search').fill('Beta');
  assert.equal(await page.locator('.workspace-link:visible').count(),1);
  await page.locator('#workspace-search').fill('');
  await page.locator('#workspace-list').evaluate(list => list.scrollTop = 700);
  const scroll = await page.locator('#workspace-list').evaluate(list=>list.scrollTop);
  await page.locator('#refresh').click(); await settled(page);
  assert.equal(await page.locator('#workspace-list').evaluate(list=>list.scrollTop),scroll);
  await page.locator('#workspace-toggle').click();
  assert.equal(await page.locator('#workspace-toggle').getAttribute('aria-expanded'),'false');
  assert.equal(await page.locator('#settings-nav').isVisible(),true);
  await page.reload(); await settled(page);
  assert.equal(await page.locator('#workspace-toggle').getAttribute('aria-expanded'),'false');
  await page.locator('#workspace-toggle').click();
  assert.equal(await page.locator('#workspace-search').isVisible(),true);
});

test('global settings work with no workspaces and do not load workspace contents', async t => {
  const {page,requests} = await setup(t,{workspaces:[]});
  await page.goto(endpoint+'/settings?workspace=missing&branch=old');
  await page.locator('#settings-form').waitFor();
  await page.waitForFunction(()=>!document.querySelector('#refresh').disabled);
  assert.equal(new URL(page.url()).search,'');
  assert.equal(await page.locator('#workspace-menu').isVisible(),false);
  assert.equal(await page.locator('#settings-nav').getAttribute('aria-current'),'page');
  assert(!requests.some(url => url.pathname.startsWith('/api/workspaces/')));
  const box = await page.locator('#settings-nav').boundingBox();
  assert(box.y > 750 && box.x < 250);
});

test('settings remain available when listing workspaces fails', async t => {
  const {page} = await setup(t,{listError:true});
  await page.goto(endpoint+'/settings');
  await page.locator('#settings-form').waitFor();
  await page.locator('#error').filter({hasText:'Workspaces unavailable'}).waitFor();
  assert.equal(await page.locator('#cache-max').inputValue(),'2');
});

test('missing workspace links fall back to the first workspace and clear scoped state', async t => {
  const {page} = await setup(t);
  await page.goto(endpoint+'/archive?workspace=missing&branch=wrong&path=%2Fwrong'); await settled(page);
  assert.equal(new URL(page.url()).search,'?workspace=first');
  assert.equal(await page.locator('#workspace-menu-name').textContent(),'项目 Alpha');
});

test('a delayed response cannot replace a newly selected workspace', async t => {
  const {page,data} = await setup(t);
  let release, entered;
  const waiting = new Promise(resolve => entered = resolve), gate = new Promise(resolve => release = resolve);
  data.pending = async id => { if(id === 'first') { entered(); await gate; } };
  await page.goto(endpoint+'/archive?workspace=first'); await waiting;
  await page.locator('[data-workspace=second]').click(); await settled(page);
  release();
  await page.waitForTimeout(100);
  assert.equal(await page.locator('#workspace-name').textContent(),'项目 Beta');
  assert.equal(new URL(page.url()).searchParams.get('workspace'),'second');
});

test('compact navigation remains reachable after resizing and keyboard collapse', async t => {
  const {page} = await setup(t);
  await page.goto(endpoint+'/'); await settled(page);
  await page.setViewportSize({width:390,height:844});
  await page.waitForFunction(()=>document.querySelector('#workspace-toggle').getAttribute('aria-expanded') === 'false');
  await page.locator('#mounts-nav').click(); await settled(page);
  await page.locator('#workspace-toggle').click();
  await page.locator('#workspace-search').fill('Beta');
  await page.locator('[data-workspace=second]').click(); await settled(page);
  assert.equal(await page.locator('#workspace-toggle').getAttribute('aria-expanded'),'false');
  await page.locator('#workspace-toggle').click(); await page.keyboard.press('Escape');
  assert.equal(await page.locator('#workspace-toggle').getAttribute('aria-expanded'),'false');
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth <= innerWidth),true);
  await page.locator('#settings-nav').click(); await page.locator('#settings-form').waitFor();
  assert.equal(await page.locator('#workspace-menu').isVisible(),false);
});


test('workspace deletion requires the name, preserves its request on failure, and selects the next archive', async t => {
  const {page,data} = await setup(t);
  await page.goto(endpoint+'/branches?workspace=first'); await settled(page);
  await page.getByRole('button',{name:'删除工作区 项目 Alpha',exact:true}).click();
  const dialog = page.locator('.workspace-delete-dialog');
  const confirm = dialog.getByRole('button',{name:'删除工作区',exact:true});
  assert.equal(await confirm.isEnabled(),false);
  await dialog.getByLabel('输入工作区名称确认删除').fill('项目 Alpha');
  data.deleteError = '请先卸载目录';
  await confirm.click(); await dialog.getByRole('alert').filter({hasText:'请先卸载目录'}).waitFor();
  assert.equal(new URL(page.url()).searchParams.get('workspace'),'first');
  const original = data.deletions[0];
  await page.locator('#refresh').evaluate(button => button.click()); await settled(page);
  assert.equal(await dialog.getByLabel('输入工作区名称确认删除').inputValue(),'项目 Alpha');
  data.deleteError = null;
  await confirm.click(); await dialog.waitFor({state:'hidden'}); await settled(page);
  assert.deepEqual(data.deletions[1],original);
  assert.equal(new URL(page.url()).pathname,'/archive');
  assert.equal(new URL(page.url()).searchParams.get('workspace'),'second');
  assert.equal(await page.locator('#workspace-total').textContent(),'1');
});

test('deleting an unselected workspace preserves the current page; deleting the last workspace shows empty state', async t => {
  const {page} = await setup(t);
  await page.goto(endpoint+'/branches?workspace=first'); await settled(page);
  for (const name of ['项目 Beta','项目 Alpha']) {
    await page.getByRole('button',{name:`删除工作区 ${name}`,exact:true}).click();
    const dialog = page.locator('.workspace-delete-dialog');
    await dialog.getByLabel('输入工作区名称确认删除').fill(name);
    await dialog.getByRole('button',{name:'删除工作区',exact:true}).click();
    await dialog.waitFor({state:'hidden'}); await settled(page);
    if (name==='项目 Beta') assert.equal(new URL(page.url()).pathname,'/branches');
  }
  assert.equal(await page.locator('#workspace-total').textContent(),'0');
  assert.equal(await page.locator('#workspace-menu-name').textContent(),'暂无工作区');
  assert.equal(new URL(page.url()).search,'');
});

test('failed remote discovery keeps local workspaces usable and cancel does not delete', async t => {
  const {page,data} = await setup(t);
  await page.route('**/api/workspaces?discover=true', route => route.fulfill({status:503,json:{error:{message:'Remote unavailable'}}}));
  await page.goto(endpoint+'/'); await settled(page);
  assert.equal(await page.locator('#workspace-total').textContent(),'2');
  assert.match(await page.locator('#error').textContent(),/Remote unavailable/);
  await page.getByRole('button',{name:'删除工作区 项目 Alpha',exact:true}).click();
  await page.locator('.workspace-delete-dialog').getByRole('button',{name:'取消',exact:true}).click();
  assert.equal(data.deletions,undefined);
});
