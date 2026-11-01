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
    const name = pathname === '/dashboard' ? 'index.html' : pathname.slice(1);
    if (!/^[a-z.-]+$/.test(name)) return response.writeHead(404).end();
    try {
      const body = await readFile(path.join(directory, name));
      response.writeHead(200, {'Content-Type':{'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml'}[path.extname(name)] || 'text/plain'}).end(body);
    } catch { response.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  endpoint = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({headless:true});
});
after(async () => { await browser?.close(); if (server) await new Promise(resolve => server.close(resolve)); });

const created = 1793525400000000000;
function revision(id, branch = 'target', parents = []) {
  return {id,branch,parents,kind:'commit',actor:'owner',created_ns:created,root:{id:'tree-'+id},operation:'apply',workspace:'workspace'};
}
function record(id = 'merge-0') {
  return {id,merge_id:id,source_branch:'source',source_name:'feat/source',source_revision:'source-revision',target_branch:'target',target_name:'feat/target',target_revision:'target-revision',base_revision:'base',candidate_revision:'candidate-2',applied_revision:'result',principal:'owner',origin_location:'node-A',created_ns:created,updated_ns:created,status:'applied',remote_confirmed:false,mount_ready:true,history_complete:true,error:null,conflict_count:0};
}
function action(id, overrides = {}) {
  return {id,operation:'operation-'+id,request_id:'request-'+id,name:'merge_resolve',principal:'owner',location:'node-A',created_ns:created,phase:'complete',local_saved:true,remote_confirmed:false,mount_ready:null,error:null,candidate_before:'candidate-1',candidate_revision:'candidate-2',applied_revision:null,conflict_count:0,paths:[{path:'/article.md',choice:'replace'}],history_complete:true,...overrides};
}
function fixture(size = 1) {
  const target = {id:'target',name:'feat/target',local:true,owner:'node-A',state:'writable',authority_epoch:1,generation:1,formal_head:'result',autosave_head:null,base_revision:'base',source_branch:null,dirty:false,mutation_seq:1,saved_seq:1,working_root:{id:'tree-result'}};
  const source = {...target,id:'source',name:'feat/source',local:false,owner:'node-B',formal_head:'source-revision'};
  const branches = [target,source];
  const revisions = [revision('base'),revision('target-revision','target',['base']),revision('source-revision','source',['base']),revision('candidate-1',null,['target-revision','source-revision']),revision('candidate-2',null,['candidate-1']),revision('result','target',['target-revision','source-revision'])];
  return {
    snapshot:{branches,revisions,status:{workspace:{id:'workspace',name:'Merge workspace'},location:'node-A',branches,mounts:[],selected_branch:'target',capabilities:{remote_configured:true},pending_jobs:0},origins:branches.map(branch => ({branch:branch.id,source_branch:null,revision:'base',revision_available:true,workspace_root:true})),summary:{branch_count:2,mount_count:0,ready_mount_count:0,mounted_branch_count:0}},
    entries:Array.from({length:size},(_,index) => record('merge-'+index)),
    events:[action('prepared',{name:'merge_prepare',candidate_before:null,candidate_revision:'candidate-1',conflict_count:1,paths:[]}),action('resolved',{created_ns:created+1e9}),action('applied',{name:'merge_apply',created_ns:created+2e9,candidate_before:null,candidate_revision:'candidate-2',applied_revision:'result',mount_ready:true,paths:[]})],
    revisions,
  };
}
function pageOf(entries, url) {
  const offset = Number(url.searchParams.get('cursor') || 0), limit = Number(url.searchParams.get('limit') || 50);
  return {entries:entries.slice(offset,offset+limit),total:entries.length,next:offset+limit < entries.length ? String(offset+limit) : null};
}
async function open(t,data = fixture(),target = '/dashboard?view=merges') {
  const page = await browser.newPage({viewport:{width:1440,height:1000}}), errors = [], requests = [];
  page.on('pageerror', error => errors.push(error.message));
  t.after(async () => { await page.close(); assert.deepEqual(errors,[]); });
  await page.addInitScript(() => sessionStorage.setItem('agentdrive-token','test-token'));
  await page.route('**/api/**',async route => {
    const url = new URL(route.request().url()); requests.push({url,method:route.request().method()});
    if (data.intercept && await data.intercept(route,url)) return;
    const pathname = url.pathname;
    if (pathname === '/api/workspaces') return route.fulfill({json:[data.snapshot.status.workspace]});
    if (pathname === '/api/workspaces/workspace') return route.fulfill({json:data.snapshot});
    if (pathname.endsWith('/history')) return route.fulfill({json:pageOf(data.revisions.filter(value => value.branch === pathname.split('/').at(-2)),url)});
    if (pathname === '/api/workspaces/workspace/merges') {
      const entries = data.entries.filter(entry => ['status','source_branch','target_branch'].every(key => !url.searchParams.get(key) || entry[key] === url.searchParams.get(key)) && (!url.searchParams.get('revision') || [entry.source_revision,entry.target_revision,entry.candidate_revision,entry.applied_revision].includes(url.searchParams.get('revision'))));
      return route.fulfill({json:{workspace:'workspace',location:'node-A',observed_ns:created,...pageOf(entries,url)}});
    }
    if (pathname.endsWith('/events')) return route.fulfill({json:{...pageOf(data.events,url),revisions:data.revisions}});
    if (pathname.includes('/merges/')) {
      const entry = data.entries.find(entry => entry.id === decodeURIComponent(pathname.split('/').at(-1)));
      if (!entry) return route.fulfill({status:404,json:{error:{message:'Merge record not found'}}});
      return route.fulfill({json:{entry,revisions:data.revisions,events:{...pageOf(data.events,new URL('http://example/')),revisions:data.revisions},location:'node-A',observed_ns:created}});
    }
    if (pathname.endsWith('/changes')) return route.fulfill({json:{total:1,next:null,entries:[{path:'/article.md',kind:'modified'}]}});
    if (pathname.endsWith('/diff')) return route.fulfill({json:{status:'text',before:{kind:'file',size:3,mode:420,uid:501,gid:20},after:{kind:'file',size:6,mode:420,uid:501,gid:20},hunks:[[{kind:'added',before_line:null,after_line:1,text:'A <script> literal',newline:true}]]}});
    return route.fulfill({status:404,json:{error:{message:'Unexpected test endpoint'}}});
  });
  await page.goto(endpoint+target);
  await page.waitForFunction(() => !document.querySelector('#refresh').disabled);
  return {page,data,requests};
}
async function refresh(page) {
  await page.evaluate(() => document.querySelector('#refresh').click());
  await page.waitForFunction(() => !document.querySelector('#refresh').disabled);
}

test('merge records load on demand and expose only reads with separate apply and remote states',async t => {
  const {page,requests} = await open(t,fixture(),'/dashboard');
  assert.equal(requests.filter(request => request.url.pathname.includes('/merges')).length,0);
  await page.getByRole('tab',{name:'合并记录',exact:true}).click();
  await page.locator('#merge-list tr').waitFor();
  assert.match(await page.locator('#merge-list').textContent(),/已应用/);
  assert.match(await page.locator('#merge-list').textContent(),/远端待确认/);
  assert.equal(requests.filter(request => request.url.pathname.includes('/merges/')).length,0);
  await page.locator('#merge-list .merge-open').click();
  await page.locator('#merge-events .merge-event').first().waitFor();
  await page.locator('#merge-dialog .diff-path').waitFor();
  assert.equal(await page.locator('#merge-events .merge-event').count(),3);
  assert.equal(await page.locator('#merge-dialog').getByText('等待价格确认',{exact:true}).count(),0);
  assert.equal(requests.every(request => request.method === 'GET'),true);
  assert(requests.some(request => request.url.pathname.endsWith('/changes') && request.url.searchParams.get('before') === 'target-revision'));
  await Promise.all([
    page.waitForResponse(response => response.url().includes('/result/changes?') && response.url().includes('before=source-revision')),
    page.getByLabel('比较版本',{exact:true}).selectOption('source-revision/result'),
  ]);
  await page.getByRole('button',{name:'查看本次候选变化'}).click();
  await page.waitForFunction(() => document.querySelector('#merge-comparison').value === 'candidate-1/candidate-2');
  await page.locator('#merge-dialog .diff-path').waitFor();
  assert.equal(await page.locator('#merge-dialog script').count(),0);
});

test('closing a detail cancels its delayed response and opening another record retains its own content',async t => {
  const data=fixture(2); data.entries[1].target_name='feat/other-target';
  let release, requested;
  const gate=new Promise(resolve=>{release=resolve;}), pending=new Promise(resolve=>{requested=resolve;});
  data.intercept=async (route,url)=>{
    if (!url.pathname.endsWith('/merges/merge-0')) return false;
    requested(); await gate;
    await route.fulfill({json:{entry:data.entries[0],revisions:data.revisions,events:{...pageOf(data.events,new URL('http://example/')),revisions:data.revisions},location:'node-A',observed_ns:created}}).catch(()=>{});
    return true;
  };
  const {page}=await open(t,data);
  await page.locator('#merge-list .merge-open').first().click(); await pending;
  await page.getByRole('button',{name:'关闭合并详情'}).click();
  await page.locator('#merge-list .merge-open').last().click();
  await page.getByRole('heading',{name:'feat/source → feat/other-target',exact:true}).waitFor();
  release();
  await page.locator('#merge-dialog .diff-path').waitFor();
  assert.equal(await page.locator('#merge-dialog-title').textContent(),'feat/source → feat/other-target');
  await page.getByRole('button',{name:'关闭合并详情'}).click();
  await page.getByRole('tab',{name:'分支与版本',exact:true}).click();
  await page.locator('#branch-tree').getByText('feat/target',{exact:true}).waitFor();
});

test('version details return keyboard focus to their merge record and Escape closes one dialog at a time',async t => {
  const {page}=await open(t);
  await page.locator('.merge-open').click();
  const version=page.locator('#merge-dialog .merge-version-link').first(); await version.waitFor();
  const trigger=await version.getAttribute('id');
  await version.click();
  await page.locator('#revision-dialog[open] .diff-path').waitFor();
  assert.equal(await page.locator('#merge-dialog').evaluate(node=>node.open),true);
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#revision-dialog').evaluate(node=>node.open),false);
  assert.equal(await page.locator('#merge-dialog').evaluate(node=>node.open),true);
  assert.equal(await page.evaluate(()=>document.activeElement.id),trigger);
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#merge-dialog').evaluate(node=>node.open),false);
  assert.equal(await page.evaluate(()=>document.activeElement.id),'merge-merge-0');
});

test('filters use branch and node choices and persist in the URL',async t => {
  const {page,requests} = await open(t);
  await page.locator('.merge-filter-extra summary').click();
  const filters = page.getByRole('form',{name:'筛选合并记录'});
  await filters.getByLabel('来源分支',{exact:true}).selectOption('source');
  await page.getByLabel('目标分支',{exact:true}).selectOption('target');
  await page.getByLabel('执行节点',{exact:true}).selectOption('node-A');
  await page.getByLabel('开始时间',{exact:true}).fill('2026-11-01T08:00');
  await page.getByLabel('合并状态',{exact:true}).selectOption('applied');
  await page.getByRole('button',{name:'查询',exact:true}).click();
  await page.waitForFunction(() => document.querySelector('#merges-status').textContent.includes('已显示'));
  const query = requests.filter(request => request.url.pathname.endsWith('/merges')).at(-1).url.searchParams;
  assert.equal(query.get('source_branch'),'source'); assert.equal(query.get('target_branch'),'target'); assert.equal(query.get('location'),'node-A'); assert.equal(query.get('status'),'applied'); assert(query.get('from_ns'));
  await refresh(page);
  assert.equal(await filters.getByLabel('来源分支',{exact:true}).inputValue(),'source');
  assert.match(page.url(),/merge_source_branch=source/);
  await page.getByRole('button',{name:'重置筛选'}).click();
  await page.waitForFunction(() => document.querySelector('#merges-status').textContent.includes('已显示'));
  assert.equal(new URL(page.url()).searchParams.has('merge_source_branch'),false);
});

test('refresh retains loaded merge rows, timeline pages, comparison and scroll position',async t => {
  const data = fixture(51); data.events = Array.from({length:101},(_,index) => action('event-'+String(index).padStart(3,'0'),{created_ns:created+index*1e6}));
  const {page} = await open(t,data);
  await page.getByRole('button',{name:'加载更多合并记录'}).click();
  await page.waitForFunction(() => document.querySelectorAll('#merge-list tr').length === 51);
  await page.locator('#merge-list tr').last().locator('button').click();
  await page.getByRole('button',{name:'加载更多合并动作'}).click();
  await page.waitForFunction(() => document.querySelectorAll('#merge-events li.merge-event').length === 100);
  await page.getByLabel('比较版本',{exact:true}).selectOption('candidate-1/candidate-2');
  await page.locator('#merge-events li').first().locator('details').evaluate(node => {node.open=true;});
  const scroll = await page.locator('#merge-dialog').evaluate(node => {node.scrollTop=260;return node.scrollTop;});
  await page.evaluate(() => {globalThis.retainedMerge=document.querySelector('#merge-list tr');globalThis.retainedEvent=document.querySelector('#merge-events li');});
  await refresh(page);
  assert.equal(await page.locator('#merge-list tr').count(),51);
  assert.equal(await page.locator('#merge-events li.merge-event').count(),100);
  assert.equal(await page.getByLabel('比较版本',{exact:true}).inputValue(),'candidate-1/candidate-2');
  assert.equal(await page.locator('#merge-events li').first().locator('details').evaluate(node => node.open),true);
  assert.equal(await page.locator('#merge-dialog').evaluate(node => node.scrollTop),scroll);
  assert.equal(await page.evaluate(() => globalThis.retainedMerge===document.querySelector('#merge-list tr') && globalThis.retainedEvent===document.querySelector('#merge-events li')),true);
});

test('a new candidate preserves the selected fixed comparison and identifies it as an earlier choice',async t => {
  const data=fixture(); Object.assign(data.entries[0],{candidate_revision:'candidate-1',applied_revision:null,status:'pending',remote_confirmed:null});
  data.events=data.events.slice(0,1);
  const {page,requests}=await open(t,data);
  await page.locator('.merge-open').click(); await page.locator('#merge-dialog .diff-path').waitFor();
  assert.equal(await page.getByLabel('比较版本',{exact:true}).inputValue(),'target-revision/candidate-1');
  const reads=requests.filter(request=>request.url.pathname.endsWith('/changes')).length;
  data.entries[0].candidate_revision='candidate-2'; data.events.push(action('resolved',{created_ns:created+1e9}));
  await refresh(page);
  assert.equal(await page.getByLabel('比较版本',{exact:true}).inputValue(),'target-revision/candidate-1');
  assert.match(await page.locator('#merge-comparison option:checked').textContent(),/先前选择/);
  assert.match(await page.locator('#merge-dialog .comparison-summary').textContent(),/先前选择/);
  assert.equal(requests.filter(request=>request.url.pathname.endsWith('/changes')).length,reads);
  await Promise.all([
    page.waitForResponse(response=>response.url().includes('/candidate-2/changes?')&&response.url().includes('before=target-revision')),
    page.getByLabel('比较版本',{exact:true}).selectOption('target-revision/candidate-2'),
  ]);
});

test('new records and failed refreshes preserve the loaded list until an explicit update',async t => {
  const {page,data} = await open(t,fixture(51));
  await page.getByRole('button',{name:'加载更多合并记录'}).click();
  await page.waitForFunction(() => document.querySelectorAll('#merge-list tr').length===51);
  data.entries.unshift(record('new-record'));
  await refresh(page);
  assert.equal(await page.locator('#merge-list tr').count(),51);
  assert.equal(await page.locator('[data-merge="new-record"]').count(),0);
  await page.getByRole('button',{name:'记录已更新，查看更新'}).click();
  await page.locator('[data-merge="new-record"]').waitFor();
  assert.equal(await page.locator('#merge-list tr').count(),52);
  assert.equal(await page.locator('#merge-list tr').first().getAttribute('data-merge'),'new-record');
  data.intercept = async (route,url) => { if (!url.pathname.endsWith('/merges')) return false; await route.fulfill({status:503,json:{error:{message:'temporary outage'}}}); return true; };
  await refresh(page);
  assert.equal(await page.locator('#merge-list tr').count(),52);
  assert.match(await page.locator('#merges-status').textContent(),/当前列表已保留/);
  delete data.intercept;
  await page.getByRole('button',{name:'重新加载合并记录'}).click();
  await page.waitForFunction(() => document.querySelector('#merges-reload').hidden);
});

test('an earlier synchronized action appears in chronological order without replacing existing timeline rows',async t=>{
  const {page,data}=await open(t);
  await page.locator('.merge-open').click(); await page.locator('#merge-dialog .diff-path').waitFor();
  await page.locator('[data-event="resolved"] details').evaluate(node=>{node.open=true;});
  data.events.push(action('arrived-later',{created_ns:created+5e8}));
  await refresh(page);
  assert.deepEqual(await page.locator('#merge-events li.merge-event').evaluateAll(rows=>rows.map(row=>row.dataset.event)),['prepared','arrived-later','resolved','applied']);
  assert.equal(await page.locator('[data-event="resolved"] details').evaluate(node=>node.open),true);
});

test('legacy failed attempts show missing data and do not fabricate a candidate or review',async t => {
  const data = fixture();
  Object.assign(data.entries[0],{id:'ab517d86-142f-47fa-a07d-615cb0007da5',merge_id:null,status:'failed',candidate_revision:null,applied_revision:null,origin_location:null,history_complete:false,remote_confirmed:null,conflict_count:null,error:{code:'TARGET_MOVED',message:'branch changed'}});
  data.events=[action('failed',{name:'merge_prepare',phase:'failed',local_saved:false,candidate_before:null,candidate_revision:null,history_complete:false,error:{code:'TARGET_MOVED',message:'branch changed'}})];
  const {page,requests}=await open(t,data);
  await page.locator('.merge-open').click();
  await page.getByText('历史步骤未完整记录；当前展示能够读取的操作和版本。').waitFor();
  assert.match(await page.locator('#merge-dialog').textContent(),/执行节点未记录/);
  assert.equal(await page.getByLabel('比较版本',{exact:true}).isVisible(),false);
  assert.equal(requests.filter(request => /\/(changes|diff)$/.test(request.url.pathname)).length,0);
  assert.equal(await page.locator('#merge-dialog').getByText('复核通过',{exact:true}).count(),0);
});

test('already included sources show the existing target and compare against it without creating a candidate',async t => {
  const data=fixture();
  Object.assign(data.entries[0],{merge_id:null,status:'already_included',candidate_revision:null,applied_revision:null,base_revision:null,remote_confirmed:null,mount_ready:null});
  data.events=[action('included',{name:'merge_prepare',candidate_before:null,candidate_revision:null,applied_revision:'target-revision',paths:[]})];
  const {page,requests}=await open(t,data);
  const result=page.locator('#merge-list tr td').last();
  assert.match(await result.textContent(),/现有目标版本 · 无需新版本/);
  assert.equal(await result.locator('code').getAttribute('title'),'target-revision');
  await page.locator('.merge-open').click();
  await page.locator('#merge-dialog .diff-path').waitFor();
  const card=page.locator('.merge-version-relation > .merge-version-card');
  assert.match(await card.textContent(),/现有目标版本/);
  assert.match(await card.textContent(),/无需生成新版本/);
  assert.equal(await card.locator('.merge-version-link').getAttribute('title'),'target-revision');
  assert.equal(await page.locator('#merge-dialog').getByText('当前候选',{exact:true}).count(),0);
  assert.equal(await page.getByLabel('比较版本',{exact:true}).inputValue(),'source-revision/target-revision');
  assert.match(await page.locator('#merge-dialog .comparison-summary').textContent(),/来源版本 → 现有目标版本/);
  assert(requests.some(request=>request.url.pathname.endsWith('/target-revision/changes')&&request.url.searchParams.get('before')==='source-revision'));
  assert.equal(requests.every(request=>request.method==='GET'),true);
});

test('merge commits link from version history and the layout fits mobile screens',async t => {
  const {page}=await open(t,fixture(),'/dashboard');
  await page.locator('#revision-result').dblclick();
  await page.getByRole('button',{name:'查看合并记录',exact:true}).click();
  await page.locator('#merge-list tr').waitFor();
  assert.equal(new URL(page.url()).searchParams.get('revision'),'result');
  assert.equal(await page.getByRole('tab',{name:'合并记录',exact:true}).getAttribute('aria-selected'),'true');
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),true);
  await page.locator('.merge-open').click();
  await page.locator('#merge-dialog .diff-path').waitFor();
  assert.equal(await page.locator('#merge-dialog').evaluate(node => node.scrollWidth <= node.clientWidth),true);
});
