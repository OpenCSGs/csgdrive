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
