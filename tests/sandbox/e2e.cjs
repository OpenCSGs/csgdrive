// Run against a real local daemon and KVM-backed Ubuntu template.
const {chromium} = require('../dashboard/node_modules/playwright');
const {execFile} = require('node:child_process');
const {promisify} = require('node:util');
const fs = require('node:fs/promises');
const path = require('node:path');
const assert = require('node:assert/strict');
const exec = promisify(execFile);
const root = path.resolve(__dirname, '../..');
const endpoint = process.env.AGENTDRIVE_E2E_URL || 'http://127.0.0.1:17421';
const configPath = process.env.AGENTDRIVE_SANDBOX_CONFIG || path.join(root,'.agentdrive/sandbox/runtime.json');
const runId = new Date().toISOString().replace(/[:.]/g,'-');
const artifacts = path.join(root,'.agentdrive/verification',`e2e-${runId}`);
async function main() {
  await fs.mkdir(artifacts,{recursive:true});
  const config=JSON.parse(await fs.readFile(configPath,'utf8'));
  const report={started_at:new Date().toISOString(),endpoint,checks:[],startup_samples:[]};
  const record=(name,detail={})=>{report.checks.push({name,...detail});console.log(name,JSON.stringify(detail));};
  const api=async(url,options={})=>{const response=await fetch(endpoint+url,{...options,signal:AbortSignal.timeout(150000)});assert(response.ok,`${response.status}: ${await response.clone().text()}`);return response;};
  const fixture=JSON.parse((await exec(path.join(root,'target/debug/agentdrive'),['--endpoint',endpoint+'/mcp','call','workspace_create','--request-id',`e2e-${runId}`,'--json',JSON.stringify({name:`Sandbox E2E ${runId}`})])).stdout);
  assert(!fixture.error,JSON.stringify(fixture));
  const workspace=fixture.workspace, sourceBranch=fixture.branch;
  report.workspace=workspace;report.source_branch=sourceBranch;
  const browser=await chromium.launch({headless:true});
  const context=await browser.newContext({viewport:{width:1440,height:1080}});
  await context.tracing.start({screenshots:true,snapshots:true,sources:true});
  const page=await context.newPage();
  const pageErrors=[];context.on('page',p=>p.on('pageerror',e=>pageErrors.push(e.message)));page.on('pageerror',e=>pageErrors.push(e.message));
  let timer;
  try {
    await page.goto(`${endpoint}/branches?workspace=${workspace}`);
    const name=`feat/e2e-${runId}`;
    await page.getByRole('textbox',{name:'分支名称',exact:true}).fill(name);
    await page.getByRole('button',{name:'创建分支',exact:true}).click();
    const row=page.locator('article.management-row').filter({has:page.getByRole('heading',{name,exact:true})});
    await row.waitFor({timeout:30000});
    const branch=await row.getAttribute('data-branch');report.branch=branch;
    const sandboxPath=`/api/workspaces/${workspace}/branches/${branch}/sandbox`;
    const filesPath=id=>`/api/workspaces/${workspace}/branches/${id}`;
    record('branch_created_in_browser',{branch});
    let sampling=false;
    timer=setInterval(async()=>{
      if(sampling)return;sampling=true;const start=Date.now();
      try {const result=await (await fetch(endpoint+sandboxPath,{signal:AbortSignal.timeout(2000)})).json();report.startup_samples.push({elapsed_ms:Date.now()-start,...result.sandbox});}catch(e){report.startup_samples.push({error:e.message});}finally{sampling=false;}
    },200);
    await row.getByRole('button',{name:'Sandbox',exact:true}).click();
    const controls=page.getByRole('dialog');
    await controls.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
    await controls.getByRole('link',{name:'打开桌面'}).waitFor({timeout:150000});
    clearInterval(timer);timer=null;
    assert.equal(await controls.locator('[data-state="complete"]').count(),4);
    assert(report.startup_samples.some(s=>['creating','starting'].includes(s.state)),'Startup status was never observable');
    assert(report.startup_samples.every(s=>!s.error),'Status polling stalled');
    await page.screenshot({path:path.join(artifacts,'branch-ready.png'),fullPage:true});
    const first=await (await api(sandboxPath)).json();
    const repeated=await (await api(sandboxPath,{method:'POST'})).json();
    assert.equal(first.sandbox.id,repeated.id);
    record('sandbox_created_with_live_progress',{steps:4,reused_id:repeated.id});
    async function openDesktop(label){
      const popupPromise=page.waitForEvent('popup');await controls.getByRole('link',{name:'打开桌面'}).click();
      const desktop=await popupPromise;await desktop.waitForFunction(()=>document.title.endsWith(' - noVNC'),null,{timeout:30000});
      await desktop.screenshot({path:path.join(artifacts,`${label}.png`)});record(label,{title:await desktop.title()});return desktop;
    }
    let desktop=await openDesktop('desktop_connected');
    async function verifyNetwork(label) {
      if (!config.networks?.length) return;
      const output=await exec('python3',[path.join(root,'tests/sandbox/network_probe.py'),'--socket',path.join(config.directory,branch,'vsock.sock'),'--artifacts',path.join(artifacts,label)],{timeout:180000,maxBuffer:4*1024*1024});
      await fs.writeFile(path.join(artifacts,`${label}.log`),output.stdout+output.stderr);
      const vm=JSON.parse(await fs.readFile(path.join(config.directory,branch,'vm.json'),'utf8'));
      record(label,{tap:vm.net[0].tap,websites:['https://example.com','https://www.python.org/']});
    }
    await verifyNetwork('browser_internet_access');
    const probeDir=path.join(artifacts,'computer-use');
    const probe=await exec('python3',[path.join(root,'tests/sandbox/desktop_probe.py'),'--socket',path.join(config.directory,branch,'vsock.sock'),'--mount',path.join(config.mount_root,branch),'--artifacts',probeDir],{timeout:90000,maxBuffer:4*1024*1024});
    await fs.writeFile(path.join(artifacts,'computer-use.log'),probe.stdout+probe.stderr);
    const result=JSON.parse(await fs.readFile(path.join(probeDir,'report.json'),'utf8'));
    async function readSaved(){
      const listing=await (await api(filesPath(branch)+'/files?path=%2F')).json();
      const query=new URLSearchParams({path:'/'+result.file,version:listing.version,offset:'0',size:'65536'});
      const content=await (await api(filesPath(branch)+'/file?'+query)).text();assert.equal(content,result.content);return content;
    }
    await readSaved();
    const sourceFiles=await (await api(filesPath(sourceBranch)+'/files?path=%2F')).json();
    assert(!JSON.stringify(sourceFiles.entries).includes(result.file),'Guest file escaped into source branch');
    record('computer_use_saved_current_branch_file',{file:result.file,content:result.content});
    record('source_branch_unchanged');
    await desktop.screenshot({path:path.join(artifacts,'desktop-file-saved.png')});await desktop.close();
    await controls.getByRole('button',{name:'停止 Sandbox'}).click();
    await controls.getByRole('button',{name:'启动 Sandbox',exact:true}).waitFor({timeout:40000});
    assert.equal((await (await api(sandboxPath)).json()).sandbox.state,'stopped');await readSaved();record('stop_preserved_branch_file');
    await controls.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
    await controls.getByRole('link',{name:'打开桌面'}).waitFor({timeout:150000});
    assert.equal((await (await api(sandboxPath)).json()).sandbox.id,repeated.id);
    desktop=await openDesktop('desktop_reconnected_after_restart');await readSaved();await desktop.close();record('restart_preserved_instance_and_file');
    await verifyNetwork('browser_internet_after_restart');
    await controls.getByRole('button',{name:'停止 Sandbox'}).click();await controls.getByRole('button',{name:'启动 Sandbox',exact:true}).waitFor({timeout:40000});
    assert.deepEqual(pageErrors,[]);report.result='passed';report.finished_at=new Date().toISOString();
  } catch(error) {report.result='failed';report.error=error.stack;await page.screenshot({path:path.join(artifacts,'failure.png')}).catch(()=>{});throw error;}
  finally {clearInterval(timer);await fs.writeFile(path.join(artifacts,'report.json'),JSON.stringify(report,null,2));await context.tracing.stop({path:path.join(artifacts,'trace.zip')});await browser.close();console.log('Artifacts:',artifacts);}
}
main().catch(error=>{console.error(error);process.exitCode=1;});
