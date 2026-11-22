export function createWorkspaceActions({api, element, changed}) {
  const dialog = element('dialog', 'revision-dialog workspace-delete-dialog');
  const title = element('h2', '', '删除工作区'); title.id = 'workspace-delete-title';
  dialog.setAttribute('aria-labelledby', title.id);
  const name = element('p', 'workspace-delete-name');
  const description = element('p', '', '删除对所有 location 生效，分支和版本将无法继续访问。其他 location 收到删除状态后会停止使用并卸载目录。');
  const label = element('label', '', '输入工作区名称确认删除');
  const input = element('input'); input.autocomplete = 'off'; label.append(input);
  const error = element('p', 'command-error'); error.setAttribute('role', 'alert');
  const actions = element('div', 'management-actions');
  const cancel = element('button', 'secondary', '取消'), confirm = element('button', 'workspace-delete-confirm', '删除工作区');
  cancel.type = confirm.type = 'button'; confirm.disabled = true;
  actions.append(cancel, confirm); dialog.append(title, name, description, label, error, actions); document.body.append(dialog);
  let target = null, busy = false, trigger = null;
  const update = () => { confirm.disabled = busy || input.value !== target?.name; cancel.disabled = busy; input.disabled = busy; };
  input.oninput = update;
  function close() { if (busy) return; dialog.close(); trigger?.focus({preventScroll:true}); target = null; }
  cancel.onclick = close;
  dialog.addEventListener('cancel', event => { event.preventDefault(); close(); });
  confirm.onclick = async () => {
    if (confirm.disabled) return;
    const workspace = target, key = `agentdrive-workspace-delete:${workspace.id}`;
    let requestId = localStorage.getItem(key);
    if (!requestId) { requestId = crypto.randomUUID(); localStorage.setItem(key, requestId); }
    busy = true; update(); error.textContent = ''; confirm.textContent = '正在删除…';
    try {
      const result = await api('/api/commands/workspace_delete', {method:'POST', headers:{'Content-Type':'application/json'},
        body:JSON.stringify({request_id:requestId,request:{workspace:workspace.id}})});
      if (result.error) throw new Error(result.error.message);
      if (!result.local_saved || result.phase !== 'complete') throw new Error('删除尚未完成，请稍后重试。');
      localStorage.removeItem(key); busy = false; close(); await changed(workspace.id);
    } catch (failure) { error.textContent = failure.message; }
    finally { busy = false; confirm.textContent = '删除工作区'; update(); }
  };
  return {show(workspace, button) {
    target = workspace; trigger = button; name.textContent = workspace.name;
    input.value = ''; error.textContent = ''; update(); dialog.showModal(); cancel.focus();
  }};
}

function storageEditor($) {
  function show(remote) {
    const backend = remote?.backend || 's3';
    $('storage-mode').value = remote && remote.enabled !== false ? backend : 'local';
    const s3 = backend === 's3' ? remote : null;
    const opencsg = backend === 'opencsg' ? remote : null;
    $('s3-endpoint').value = s3?.endpoint || '';
    $('s3-bucket').value = s3?.bucket || '';
    $('s3-region').value = s3?.region || 'us-east-1';
    $('s3-prefix').value = s3?.prefix || 'agentdrive/v1';
    $('s3-http').checked = s3?.allow_http || false;
    $('s3-auth').value = s3?.credentials ? 'saved' : 'environment';
    $('s3-access-key').value = s3?.credentials?.access_key_id || '';
    $('s3-secret-key').value = ''; $('s3-session-token').value = '';
    $('opencsg-endpoint').value = opencsg?.endpoint || '';
    $('opencsg-repository').value = opencsg?.repository || '';
    $('opencsg-branch').value = opencsg?.branch || 'main';
    $('opencsg-prefix').value = opencsg?.prefix || 'agentdrive/v1';
    $('opencsg-http').checked = opencsg?.allow_http || false;
    $('opencsg-xnet').checked = opencsg?.xnet || false;
    $('opencsg-token').value = '';
    update();
  }
  function read() {
    const mode = $('storage-mode').value;
    let remote = null;
    if (mode === 's3') {
      const bucket = $('s3-bucket').value.trim();
      if (!bucket) throw new Error('请填写 S3 Bucket。');
      remote = {
        backend:'s3', enabled:true, bucket, endpoint:$('s3-endpoint').value.trim() || null,
        region:$('s3-region').value.trim(), prefix:$('s3-prefix').value.trim(), allow_http:$('s3-http').checked,
        credentials:$('s3-auth').value === 'saved' ? {
          access_key_id:$('s3-access-key').value.trim(), secret_access_key:$('s3-secret-key').value,
          session_token:$('s3-session-token').value || null,
        } : null,
      };
    } else if (mode === 'opencsg') {
      remote = {backend:'opencsg', enabled:true, endpoint:$('opencsg-endpoint').value.trim(),
        repository:$('opencsg-repository').value.trim(), branch:$('opencsg-branch').value.trim(),
        prefix:$('opencsg-prefix').value.trim(), xnet:$('opencsg-xnet').checked, token:$('opencsg-token').value, allow_http:$('opencsg-http').checked};
      if (!remote.endpoint || !remote.repository) throw new Error('请填写 OpenCSG 域名和仓库。');
    }
    return remote;
  }
  function update() {
    const mode = $('storage-mode').value;
    $('s3-fields').hidden = mode !== 's3';
    $('s3-fields').disabled = mode !== 's3';
    $('opencsg-fields').hidden = mode !== 'opencsg';
    $('opencsg-fields').disabled = mode !== 'opencsg';
    for (const name of ['bucket', 'region', 'prefix']) $('s3-' + name).required = mode === 's3';
    const saved = mode === 's3' && $('s3-auth').value === 'saved';
    for (const id of ['s3-access-key', 's3-secret-key', 's3-session-token']) $(id).disabled = !saved;
    $('s3-access-key').required = saved;
    for (const name of ['endpoint','repository','branch','prefix']) $('opencsg-'+name).required = mode === 'opencsg';
    $('s3-secret-key').required = saved;
    $('opencsg-token').required = mode === 'opencsg';
  }
  return {show, read, update};
}

export function createWorkspaceCreator({api, element, created}) {
  const dialog = element('dialog', 'revision-dialog workspace-create-dialog');
  const title = element('h2', '', '创建工作区'); title.id = 'workspace-create-title';
  dialog.setAttribute('aria-labelledby', title.id);
  const description = element('p', '', '选择文件和版本记录的存储位置。工作区创建后，存储位置固定。');
  const form = element('form');
  const label = element('label', '', '工作区名称');
  const input = element('input'); input.required = true; input.autocomplete = 'off'; input.placeholder = '例如：市场调研'; label.append(input);
  const error = element('p', 'command-error'); error.setAttribute('role', 'alert');
  const note = element('p', 'command-note'); note.hidden = true;
  const actions = element('div', 'management-actions');
  const cancel = element('button', 'secondary', '取消'), submit = element('button', 'primary', '创建工作区');
  cancel.type = 'button'; submit.type = 'submit'; actions.append(cancel, submit);
  const storage = document.getElementById('create-storage-template').content.firstElementChild.cloneNode(true);
  const editor = storageEditor(id => storage.querySelector('#create-' + id));
  storage.addEventListener('input', () => { editor.update(); error.textContent = ''; });
  form.append(label, storage, error, note, actions); dialog.append(title, description, form); document.body.append(dialog);
  const key = 'agentdrive-workspace-create';
  let ticket = null, busy = false, trigger = null;
  function update() {
    const name = input.value.trim();
    const valid = name.length > 0 && new TextEncoder().encode(name).length <= 256 && !/[\p{Cc}]/u.test(name);
    input.setCustomValidity(valid ? '' : '请输入工作区名称，长度不超过 256 字节，且不包含控制字符。');
    submit.disabled = busy || !valid; cancel.disabled = busy;
    input.readOnly = busy || !!ticket;
    for (const field of storage.querySelectorAll('select, input[type=checkbox]')) field.disabled = busy || !!ticket;
    for (const field of storage.querySelectorAll('input:not([type=checkbox])')) {
      field.readOnly = busy || (!!ticket && field.type !== 'password');
    }
    submit.textContent = busy ? '正在创建…' : ticket ? '重试创建' : '创建工作区';
    note.hidden = !ticket;
    note.textContent = ticket ? '创建结果尚未确认。重试会继续同一个请求，重新打开表单后需再次填写密钥。' : '';
  }
  function clearTicket() { ticket = null; sessionStorage.removeItem(key); }
  function close() { if (busy) return; dialog.close(); trigger?.focus({preventScroll:true}); }
  input.oninput = () => { error.textContent = ''; update(); };
  cancel.onclick = close;
  dialog.addEventListener('cancel', event => { event.preventDefault(); close(); });
  form.onsubmit = async event => {
    event.preventDefault(); update(); if (submit.disabled) return;
    busy = true; error.textContent = '';
    try {
      const profile = editor.read();
      const request = {request_id:ticket?.request_id || crypto.randomUUID(), name:input.value.trim(), storage:profile};
      ticket = structuredClone(request);
      if (ticket.storage?.backend === 'opencsg') delete ticket.storage.token;
      if (ticket.storage?.credentials) {
        delete ticket.storage.credentials.secret_access_key;
        delete ticket.storage.credentials.session_token;
      }
      sessionStorage.setItem(key, JSON.stringify(ticket)); update();
      const result = await api('/api/workspaces', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(request)});
      if (result.error && !result.local_saved) {
        if (['failed','cancelled'].includes(result.phase)) clearTicket();
        throw new Error(result.error.message);
      }
      if (!result.local_saved || !result.result?.workspace?.id) throw new Error('创建尚未确认，请重试同一个请求。');
      clearTicket(); busy = false; close(); created(result.result.workspace);
    } catch (failure) {
      if (failure.status >= 400 && failure.status < 500 && failure.status !== 408) clearTicket();
      error.textContent = failure.message;
    } finally { busy = false; update(); }
  };
  return {show(button) {
    trigger = button;
    try { ticket = JSON.parse(sessionStorage.getItem(key)); } catch { ticket = null; }
    if (!ticket?.request_id || typeof ticket.name !== 'string') ticket = null;
    editor.show(ticket?.storage);
    input.value = ticket?.name || ''; error.textContent = ''; update(); dialog.showModal(); input.focus();
  }};
}
