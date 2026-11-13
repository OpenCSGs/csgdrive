// Run against a local daemon with one available Sandbox slot.
const {chromium} = require('../dashboard/node_modules/playwright');
const {execFile} = require('node:child_process');
const {promisify} = require('node:util');
const fs = require('node:fs/promises');
const path = require('node:path');
const assert = require('node:assert/strict');
const {randomUUID} = require('node:crypto');
const root = path.resolve(__dirname, '../..');
const base = process.env.AGENTDRIVE_E2E_URL || 'http://127.0.0.1:17421';
const artifacts = path.join(root, '.agentdrive/verification/sandbox-delete');
async function request(url, method='GET', body) {
  const response = await fetch(base+url, {method, headers:{Origin:base,'Content-Type':'application/json'}, body:body===undefined?undefined:JSON.stringify(body), signal:AbortSignal.timeout(150000)});
  const value = await response.json();
  assert(response.ok, JSON.stringify({status:response.status,value})); return value;
}
async function main() {
  await fs.mkdir(artifacts,{recursive:true});
  const config = JSON.parse(await fs.readFile(path.join(root,'.agentdrive/sandbox/runtime.json'),'utf8'));
  const templateBefore = await fs.stat(config.image);
  const fixture = JSON.parse((await promisify(execFile)(path.join(root,'target/debug/agentdrive'), ['--endpoint',base+'/mcp','call','workspace_create','--request-id',randomUUID(),'--json',JSON.stringify({name:`Sandbox deletion E2E ${new Date().toISOString()}`})])).stdout);
  assert(!fixture.error,JSON.stringify(fixture));
  const {workspace,branch} = fixture;
  const endpoint = `/api/workspaces/${workspace}/branches/${branch}/sandbox`;
  const directory = path.join(config.directory,branch), mount = path.join(config.mount_root,branch);
  const report = {workspace,branch,checks:[]};
  const record = name => {report.checks.push(name); console.log(name);};
  const browser = await chromium.launch({headless:true});
  const page = await browser.newPage({viewport:{width:1440,height:1000}});
  const errors=[]; page.on('pageerror',e=>errors.push(e.message));
  try {
    await request(endpoint,'POST');
    const marker = `preserved branch file ${randomUUID()}\n`;
    await fs.mkdir(path.join(mount,'Downloads'),{recursive:true});
    await fs.writeFile(path.join(mount,'Downloads/delete-preservation.txt'),marker);
    const rejected = await fetch(base+endpoint+'/delete',{method:'POST',headers:{Origin:base,'Content-Type':'application/json'},body:JSON.stringify({confirm_branch:branch})});
    assert(!rejected.ok); assert.equal((await request(endpoint)).sandbox.state,'ready');
    record('running_instance_deletion_rejected');
    await request(endpoint,'DELETE');
    const before = await request(`/api/workspaces/${workspace}`);
    const history = before.revisions.map(r=>r.id);
    await page.goto(base+'/sandboxes');
    const card = page.locator(`.sandbox-card[data-branch="${branch}"]`);
    await card.getByRole('button',{name:'删除 Sandbox',exact:true}).click();
    const dialog = page.getByRole('dialog',{name:'删除 Sandbox',exact:true});
    await dialog.getByRole('textbox').fill('main');
    await page.screenshot({path:path.join(artifacts,'confirmation.png'),fullPage:true});
    await dialog.getByRole('button',{name:'取消'}).click();
    await fs.access(path.join(directory,'ubuntu.ext4')); record('cancellation_preserves_disk');
    await card.getByRole('button',{name:'删除 Sandbox',exact:true}).click();
    await dialog.getByRole('textbox').fill('main');
    const deleted = page.waitForResponse(r=>r.url().endsWith(endpoint+'/delete') && r.request().method()==='POST');
    await dialog.getByRole('button',{name:'确认删除'}).click();
    assert((await deleted).ok()); await card.waitFor({state:'hidden'});
    await assert.rejects(fs.access(directory));
    assert.equal((await request(endpoint)).sandbox,null);
    assert.equal(await fs.readFile(path.join(mount,'Downloads/delete-preservation.txt'),'utf8'),marker);
    const after = await request(`/api/workspaces/${workspace}`);
    assert(after.branches.some(b=>b.id===branch));
    assert(history.every(id=>after.revisions.some(r=>r.id===id)));
    assert(after.status.mounts.some(m=>m.branch===branch && m.state==='ready'));
    record('deletion_removes_disk_and_preserves_branch_files_history_and_mount');
    const templateAfter = await fs.stat(config.image);
    assert.equal(templateAfter.mtimeMs,templateBefore.mtimeMs); assert.equal(templateAfter.size,templateBefore.size);
    record('template_unchanged');
    await page.getByRole('combobox',{name:'筛选沙盒状态'}).selectOption('idle');
    await card.getByRole('button',{name:'启动 Sandbox',exact:true}).click();
    await card.getByRole('link',{name:'打开桌面'}).waitFor({timeout:150000});
    await fs.access(path.join(directory,'ubuntu.ext4'));
    assert.equal(await fs.readFile(path.join(mount,'Downloads/delete-preservation.txt'),'utf8'),marker);
    record('recreation_uses_template_and_retains_branch_file');
    assert.deepEqual(errors,[]);
    await request(endpoint,'DELETE'); await request(endpoint+'/delete','POST',{confirm_branch:branch});
    record('test_instance_cleaned_up');
    await page.getByRole('combobox',{name:'筛选沙盒状态'}).selectOption('existing');
    await page.screenshot({path:path.join(artifacts,'catalog.png'),fullPage:true});
    report.passed=true;
  } finally {
    await fs.writeFile(path.join(artifacts,'report.json'),JSON.stringify(report,null,2));
    await browser.close();
  }
}
main().catch(error=>{console.error(error);process.exitCode=1;});
