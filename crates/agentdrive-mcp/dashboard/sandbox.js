const startupSteps = [
  ['prepare_branch', '准备当前分支目录'],
  ['prepare_disk', '准备 Ubuntu 系统磁盘'],
  ['start_vm', '启动 Ubuntu 实例'],
  ['wait_desktop', '等待桌面启动'],
];

export function createSandboxControl({api, element, workspace, branch, branchName = branch, workspaceName = workspace, inline = false, initial, templates = [], canStart = true, onDeleted = () => {}}) {
  const root = element('div', 'sandbox-controls');
  const trigger = element('button', 'secondary sandbox-trigger'); trigger.type = 'button';
  trigger.id = `sandbox-trigger-${workspace}-${branch}`;
  trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-label', 'Sandbox');
  const dot = element('span', 'sandbox-dot'); dot.setAttribute('aria-hidden', 'true');
  trigger.append(dot, element('span', '', 'Sandbox')); root.append(trigger);
  const dialog = element('dialog', 'revision-dialog sandbox-dialog');
  const titleId = `sandbox-title-${workspace}-${branch}`;
  dialog.setAttribute('aria-labelledby', titleId);
  const header = element('div', 'revision-dialog-heading sandbox-heading');
  const heading = element('div');
  heading.append(element('div', 'sandbox-eyebrow', 'UBUNTU GNOME'));
  const title = element('h2', '', 'Sandbox'); title.id = titleId;
  const identity = element('p', 'sandbox-identity', branchName); identity.title = branch;
  heading.append(title, identity);
  const close = element('button', 'revision-dialog-close', '×'); close.type = 'button'; close.setAttribute('aria-label', '关闭 Sandbox');
  header.append(heading, close);
  const body = element('div', 'sandbox-body');
  const overview = element('section', 'sandbox-overview');
  const overviewText = element('div');
  const badge = element('span', 'sandbox-badge', '读取状态中');
  const status = element('p', 'sandbox-status'); status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
  overviewText.append(badge, status);
  const actions = element('div', 'sandbox-instance-actions');
  const launchActions = element('div', 'sandbox-launch-actions');
  const manageActions = element('div', 'sandbox-manage-actions');
  const create = element('button', 'primary', '启动 Sandbox'); create.type = 'button'; create.disabled = true;
  const stop = element('button', 'secondary sandbox-stop', '停止 Sandbox'); stop.type = 'button'; stop.hidden = true;
  const force = element('button', 'secondary sandbox-stop', '强制停止'); force.type = 'button'; force.hidden = true;
  const desktop = element('a', 'primary sandbox-desktop', '打开桌面'); desktop.hidden = true;
  desktop.target = '_blank'; desktop.rel = 'noopener';
  const remove = element('button', 'secondary sandbox-stop', '删除 Sandbox'); remove.type = 'button'; remove.hidden = true;
  const serviceActions = element('div', 'sandbox-service-actions');
  launchActions.append(create, desktop, serviceActions); manageActions.append(stop, force, remove);
  actions.append(launchActions, manageActions); overview.append(overviewText, actions);
  const quickActions = inline ? element('div', 'sandbox-instance-actions') : null;
  const quickLaunch = element('div', 'sandbox-launch-actions');
  const quickManage = element('div', 'sandbox-manage-actions');
  const quickCreate = create.cloneNode(true), quickStop = stop.cloneNode(true), quickForce = force.cloneNode(true), quickDesktop = desktop.cloneNode(true), quickRemove = remove.cloneNode(true);
  if (quickActions) {
    trigger.querySelector('span:last-child').textContent = '详情与 MCP';
    trigger.setAttribute('aria-label', '详情与 MCP');
    quickLaunch.append(quickCreate, quickDesktop);
    quickManage.append(trigger, quickStop, quickForce, quickRemove);
    quickActions.append(quickLaunch, quickManage); root.append(quickActions);
  }
  const templateChoice = element('label', 'management-field sandbox-template-choice');
  const templateSelect = element('select'); templateSelect.setAttribute('aria-label', 'Sandbox 模板');
  templateChoice.append(element('span', '', '创建模板'), templateSelect);
  function setTemplates(values) {
    const selected = templateSelect.value;
    templates = values;
    templateSelect.replaceChildren(...values.map(value => { const option = element('option', '', value.manifest?.name || value.id); option.value = value.id; return option; }));
    if (values.some(value => value.id === selected)) templateSelect.value = selected;
    else if (values.some(value => value.id === 'ubuntu-csgclaw')) templateSelect.value = 'ubuntu-csgclaw';
    templateChoice.hidden = !!current || values.length < 2;
  }
  const proxy = element('details', 'sandbox-section'); proxy.hidden = true;
  proxy.append(element('summary', '', '代理配置'));
  const proxyBody = element('form', 'sandbox-section-body sandbox-proxy-form');
  const proxyLabel = element('label', 'management-field');
  const proxyUrl = element('input'); proxyUrl.type = 'url'; proxyUrl.disabled = true; proxyUrl.placeholder = 'http://127.0.0.1:7890'; proxyUrl.setAttribute('aria-label', 'HTTP 代理地址');
  proxyLabel.append(element('span', '', 'HTTP 代理地址'), proxyUrl);
  const saveProxy = element('button', 'secondary', '保存代理配置'); saveProxy.type = 'submit'; saveProxy.disabled = true;
  const proxyFeedback = element('p', 'sandbox-feedback'); proxyFeedback.setAttribute('role', 'status');
  proxyBody.append(element('p', '', '支持 HTTP 代理及 HTTPS CONNECT。地址从宿主机访问，留空表示直接联网。保存后，下次启动沙盒时应用到助手和 Chromium。'), proxyLabel, saveProxy, proxyFeedback);
  proxy.append(proxyBody);
  let proxyLoaded = false;
  proxy.ontoggle = async () => {
    if (!proxy.open || proxyLoaded) return;
    saveProxy.disabled = true;
    try { const config = await api(`${endpoint}/proxy`); proxyUrl.value = config.url || ''; proxyLoaded = true; }
    catch (error) { proxyFeedback.textContent = error.message; }
    finally { saveProxy.disabled = !proxyLoaded; proxyUrl.disabled = !proxyLoaded; }
  };
  proxyBody.onsubmit = async event => {
    event.preventDefault(); saveProxy.disabled = true;
    try {
      await api(`${endpoint}/proxy`, {method:'PUT', headers:{'Content-Type':'application/json'}, body:JSON.stringify({url:proxyUrl.value.trim() || null})});
      proxyFeedback.textContent = '已保存，下次启动沙盒时生效。';
    } catch (error) { proxyFeedback.textContent = error.message; }
    finally { saveProxy.disabled = false; }
  };
  const progress = element('details', 'sandbox-section');
  const progressTitle = element('summary');
  const progressCount = element('span', 'sandbox-section-meta');
  progressTitle.append(element('span', '', '启动步骤'), progressCount);
  const steps = element('ol', 'sandbox-steps'); steps.setAttribute('aria-label', 'Sandbox 启动步骤');
  progress.append(progressTitle, steps);
  const mcp = element('details', 'sandbox-section sandbox-mcp');
  const mcpTitle = element('summary'); mcpTitle.append(element('span', '', 'MCP 配置'), element('span', 'sandbox-section-meta', '连接你的 Agent'));
  const mcpBody = element('div', 'sandbox-section-body');
  mcpBody.append(element('p', '', '在宿主机的 Agent 客户端中添加这一份配置，即可同时使用 Playwright 浏览器工具和 computer-use-linux 桌面工具。两者操作 VNC 中的同一个 Chromium，无需额外添加 MCP 配置。'));
  const config = element('textarea', 'sandbox-config'); config.readOnly = true; config.spellcheck = false; config.rows = 10;
  config.setAttribute('aria-label', 'Sandbox MCP 配置'); config.hidden = true;
  const configActions = element('div', 'sandbox-config-actions');
  const copy = element('button', 'secondary', '复制配置'); copy.type = 'button'; copy.disabled = true;
  const download = element('button', 'secondary', '下载 JSON'); download.type = 'button'; download.disabled = true;
  const retry = element('button', 'secondary', '重新获取'); retry.type = 'button'; retry.hidden = true;
  const feedback = element('p', 'sandbox-feedback'); feedback.setAttribute('aria-live', 'polite');
  configActions.append(copy, download, retry);
  mcpBody.append(config, configActions, feedback, element('p', 'sandbox-note', '请保持 Sandbox 运行。实例重新启动后，在 Agent 客户端中重新连接 MCP。配置仅适用于运行 AgentDrive 的同一台主机。'));
  mcp.append(mcpTitle, mcpBody);
  const guide = element('section', 'sandbox-guide');
  function help(title, description) { const item = element('div'); item.append(element('h3', '', title), element('p', '', description)); guide.append(item); }
  help('当前分支文件', '分支自动挂载到 /workspace，桌面的 AgentDrive 文件夹指向此目录。保存到这里的文件会写入当前分支。');
  help('浏览器与下载', '网页、标签页、按钮和表单使用 browser_* 工具；终端和系统窗口使用桌面工具。Chromium 下载保存到 /workspace/Downloads，文件自动写入当前分支。模板带有 CSGClaw 时，可以点击「打开助手」直接聊天；外部 Agent 也可以使用统一 MCP。');
  body.append(overview, templateChoice, proxy, progress, mcp, guide); dialog.append(header, body);
  const endpoint = `/api/workspaces/${encodeURIComponent(workspace)}/branches/${encodeURIComponent(branch)}/sandbox`;
  const deleteDialog = element('dialog', 'revision-dialog sandbox-delete-dialog');
  const deleteTitle = element('h2', '', '删除 Sandbox'); deleteTitle.id = `${titleId}-delete`;
  deleteDialog.setAttribute('aria-labelledby', deleteTitle.id);
  const deleteIdentity = element('p', 'sandbox-identity');
  const confirmationLabel = element('label');
  const confirmationText = element('span');
  const confirmation = element('input'); confirmation.autocomplete = 'off'; confirmation.setAttribute('aria-label', '输入分支名称确认删除');
  confirmationLabel.append(confirmationText, confirmation);
  const deleteError = element('p', 'sandbox-error'); deleteError.setAttribute('role', 'alert');
  const deleteActions = element('div', 'sandbox-actions');
  const cancelDelete = element('button', 'secondary', '取消'); cancelDelete.type = 'button';
  const confirmDelete = element('button', 'primary sandbox-delete-confirm', '确认删除'); confirmDelete.type = 'button';
  deleteActions.append(cancelDelete, confirmDelete);
  const deleteDescription = element('p');
  const deletePreserved = element('p');
  deleteDialog.append(deleteTitle, deleteIdentity,
    deleteDescription, deletePreserved,
    confirmationLabel, deleteError, deleteActions);
  let busy = false, deleting = false, reading = false, disposed = false, timer = null, current = null, enabled = true, epoch = 0, configLoading = false;
  let confirmationAction = 'delete';
  let stopFailed = false;
  function updateConfirmation() { confirmDelete.disabled = busy || (confirmationAction === 'delete' ? remove.disabled : force.disabled) || confirmation.value !== branchName; }
  function schedule() {
    clearTimeout(timer);
    if (!disposed && (dialog.open || busy || ['creating', 'starting', 'stopping'].includes(current?.state))) {
      timer = setTimeout(refresh, 1500);
    }
  }
  function show(value, available = enabled) {
    const previous = current?.state;
    current = value; enabled = available;
    const state = value?.state;
    templateChoice.hidden = !!value || templates.length < 2;
    proxy.hidden = !(value?.services || []).some(service => service.id === 'csgclaw');
    serviceActions.replaceChildren();
    if (state === 'ready') for (const service of value.services || []) {
      if (service.id !== 'csgclaw') continue;
      const link = element('a', 'secondary sandbox-service-link', service.label);
      const url = new URL(location.href); url.hostname = `${service.id}.${workspace}.${branch}.localhost`; url.pathname = '/'; url.search = ''; url.hash = '/new?agent=agent-agentdrive';
      link.href = url.href; link.target = '_blank'; link.rel = 'noopener'; serviceActions.append(link);
    }
    serviceActions.hidden = !serviceActions.childElementCount;
    if (quickActions) {
      quickActions.querySelector('.sandbox-service-actions')?.remove();
      quickLaunch.append(serviceActions.cloneNode(true));
    }
    const active = ['creating', 'starting', 'stopping'].includes(state);
    create.disabled = !enabled || !canStart || busy || active;
    create.title = canStart ? '' : '当前分支尚未取得本机写入权限';
    create.hidden = ['creating', 'starting', 'ready', 'stopping'].includes(state);
    create.textContent = state === 'error' ? '重试启动' : '启动 Sandbox';
    stop.hidden = state !== 'ready'; stop.disabled = busy;
    force.hidden = state !== 'error' && !(state === 'ready' && (stopFailed || value?.message?.startsWith('正常停止失败'))); force.disabled = busy || !enabled || force.hidden;
    remove.hidden = !['stopped', 'error'].includes(state); remove.disabled = !enabled || busy || remove.hidden;
    remove.title = ['stopped', 'error'].includes(state) ? '删除系统磁盘，保留分支文件' : '请先停止 Sandbox';
    updateConfirmation();
    desktop.hidden = state !== 'ready' || !value?.desktop_url;
    let invalidDesktop = false;
    if (!desktop.hidden) {
      try {
        const url = new URL(value.desktop_url, location.origin);
        if (url.origin !== location.origin) throw new Error();
        desktop.href = url.href;
      } catch { desktop.hidden = true; invalidDesktop = true; }
    }
    launchActions.hidden = quickLaunch.hidden = create.hidden && desktop.hidden && serviceActions.hidden;
    manageActions.hidden = stop.hidden && force.hidden && remove.hidden;
    const labels = {creating:'正在准备',starting:'启动中',ready:'运行中',stopping:'停止中',stopped:'已停止',error:'启动失败'};
    const label = !enabled ? '未配置' : labels[state] || '未启动';
    badge.textContent = label; badge.dataset.state = state || 'idle'; dot.dataset.state = state || 'idle';
    trigger.title = `Sandbox · ${label}`;
    status.textContent = !enabled ? 'Sandbox 运行环境尚未配置' : invalidDesktop ? '桌面地址无效' : value?.message || ({ready:'Ubuntu 桌面已就绪，可以打开桌面或连接 Agent。',stopped:'系统磁盘和分支文件已保留，可以继续启动。',stopping:'正在保存分支文件并停止 Ubuntu。',creating:'正在准备分支目录和 Ubuntu 环境。',starting:'启动完成后即可打开桌面。'}[state]) || '启动独立 Ubuntu 桌面，操作当前分支文件。';
    status.classList.toggle('sandbox-error', state === 'error' || invalidDesktop);
    const index = state === 'ready' ? startupSteps.length : ['stopped', 'stopping'].includes(state) ? -1 : startupSteps.findIndex(([id]) => id === value?.startup_step);
    progressCount.textContent = state === 'stopped' ? '下次启动时更新' : `${Math.max(index, 0)} / ${startupSteps.length}`;
    if (previous !== state && ['creating', 'starting', 'error'].includes(state)) progress.open = true;
    steps.replaceChildren(...startupSteps.map(([id, label], position) => {
      const stepState = position < index ? 'complete' : position === index ? state === 'error' ? 'error' : 'active' : 'pending';
      const row = element('li', `sandbox-step ${stepState}`); row.dataset.step = id; row.dataset.state = stepState;
      row.append(element('span', 'sandbox-step-marker', stepState === 'complete' ? '✓' : stepState === 'error' ? '!' : String(position + 1)), element('span', '', label), element('span', 'sandbox-step-state', {complete:'已完成',active:'进行中',error:'失败',pending:'等待'}[stepState]));
      return row;
    }));
    if (quickActions) {
      for (const [source, target] of [[create, quickCreate], [stop, quickStop], [force, quickForce], [desktop, quickDesktop], [remove, quickRemove]]) {
        target.textContent = source.textContent; target.hidden = source.hidden;
        if (source.tagName === 'BUTTON') { target.disabled = source.disabled; target.title = source.title; }
        else if (!source.hidden) target.href = source.href;
      }
    }
    schedule();
  }
  async function refresh() {
    if (reading || disposed || deleting) return;
    reading = true; const version = epoch;
    try { const result = await api(endpoint); if (!disposed && version === epoch) show(result.sandbox, result.enabled); }
    catch (error) { if (!disposed && version === epoch) { status.textContent = error.message; badge.textContent = '状态获取失败'; create.disabled = true; quickCreate.disabled = true; } }
    finally { reading = false; schedule(); }
  }
  async function run(method, templateId) {
    if (busy || disposed) return;
    stopFailed = false;
    busy = true; epoch++;
    show(method === 'POST' ? {...current,state:'creating',startup_step:'prepare_branch'} : {...current,state:'stopping'});
    try { const result = await api(endpoint, {method, ...(templateId ? {headers:{"Content-Type":"application/json"}, body:JSON.stringify({template_id:templateId})} : {})}); busy = false; epoch++; if (!disposed) show(result); }
    catch (error) {
      busy = false; epoch++;
      if (disposed) return;
      if (method === 'DELETE') {
        stopFailed = true;
        try { const result = await api(endpoint); if (!disposed) show({...result.sandbox,message:error.message}, result.enabled); return; }
        catch { /* 状态无法读取时保留失败信息和强制停止入口。 */ }
      }
      if (!disposed) show({...current,state:'error',message:error.message});
    }
  }
  async function loadConfig() {
    if (configLoading || disposed) return;
    configLoading = true; feedback.textContent = '正在获取配置…'; retry.hidden = true;
    try {
      const result = await api(`${endpoint}/mcp-config`);
      if (disposed) return;
      config.value = JSON.stringify(result, null, 2); config.hidden = false;
      copy.disabled = false; download.disabled = false; feedback.textContent = '';
    } catch (error) { if (!disposed) { feedback.textContent = error.message; retry.hidden = false; } }
    finally { configLoading = false; }
  }
  function openConfirmation(action) {
    if ((action === 'delete' ? remove.disabled : force.disabled) || busy) return;
    confirmationAction = action;
    const removing = action === 'delete';
    deleteTitle.textContent = removing ? '删除 Sandbox' : '强制停止 Sandbox';
    confirmDelete.textContent = removing ? '确认删除' : '确认强制停止';
    deleteDescription.textContent = removing ? '删除后无法恢复 Ubuntu 系统磁盘、已安装的软件、浏览器登录状态和实例日志。' : '强制停止会终止 Ubuntu 进程，尚未同步的修改可能丢失。正常停止失败时可以使用此操作。';
    deletePreserved.textContent = removing ? '当前分支、版本历史以及 /workspace 中的全部文件（包括 Downloads）会保留。之后可以使用当前模板重新创建 Sandbox。' : '系统磁盘和已写入当前分支的文件会保留，停止后可以重新启动。';
    deleteIdentity.textContent = `工作区：${workspaceName} · 分支：${branchName} (${branch})`;
    confirmationText.textContent = `请输入分支名称 ${branchName} 确认${removing ? '删除' : '强制停止'}`;
    confirmation.setAttribute('aria-label', removing ? '输入分支名称确认删除' : '输入分支名称确认强制停止');
    confirmation.value = ''; deleteError.textContent = ''; updateConfirmation();
    if (!deleteDialog.isConnected) document.body.append(deleteDialog);
    deleteDialog.showModal(); confirmation.focus();
  }
  remove.onclick = () => openConfirmation('delete');
  force.onclick = () => openConfirmation('force-stop');
  quickRemove.onclick = remove.onclick;
  quickForce.onclick = force.onclick;
  confirmation.oninput = updateConfirmation;
  cancelDelete.onclick = () => deleteDialog.close();
  deleteDialog.addEventListener('cancel', event => { if (deleting) event.preventDefault(); });
  confirmDelete.onclick = async () => {
    if (confirmDelete.disabled || busy) return;
    busy = true; deleting = true; epoch++; show(current);
    const removing = confirmationAction === 'delete';
    confirmation.disabled = true; cancelDelete.disabled = true; confirmDelete.textContent = removing ? '正在删除…' : '正在强制停止…'; deleteError.textContent = '';
    let result;
    try { result = await api(`${endpoint}/${confirmationAction}`, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({confirm_branch:branch})}); }
    catch (error) {
      busy = false; deleting = false; epoch++;
      if (!disposed) { deleteError.textContent = error.message; confirmation.disabled = false; cancelDelete.disabled = false; confirmDelete.textContent = removing ? '确认删除' : '确认强制停止'; show(current); }
      return;
    }
    busy = false; deleting = false; epoch++;
    if (disposed) return;
    confirmation.disabled = false; cancelDelete.disabled = false; confirmDelete.textContent = '确认删除';
    if (!removing) { show(result); deleteDialog.close(); return; }
    config.value = ''; config.hidden = true; copy.disabled = true; download.disabled = true; mcp.open = false;
    show(null); deleteDialog.close(); dialog.close(); onDeleted();
  };
  mcp.ontoggle = () => { if (mcp.open && !config.value) loadConfig(); };
  retry.onclick = loadConfig;
  copy.onclick = async () => {
    try { await navigator.clipboard.writeText(config.value); feedback.textContent = '配置已复制'; }
    catch { config.focus(); config.select(); feedback.textContent = '请按 Ctrl+C 或 ⌘C 复制选中的配置。'; }
  };
  download.onclick = () => {
    const url = URL.createObjectURL(new Blob([config.value + '\n'], {type:'application/json'}));
    const link = document.createElement('a'); link.href = url; link.download = `sandbox-mcp-${branch}.json`; link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  function open() {
    if (!dialog.isConnected) document.body.append(dialog);
    if (!dialog.open) dialog.showModal();
    refresh();
    if (!current && !templates.length) api('/api/sandboxes').then(data => { if (!disposed && data.templates) setTemplates(data.templates); }).catch(() => {});
  }
  trigger.onclick = open;
  close.onclick = () => dialog.close();
  dialog.addEventListener('click', event => {
    const bounds = dialog.getBoundingClientRect();
    if (event.target === dialog && (event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom)) dialog.close();
  });
  dialog.addEventListener('close', () => { if (!disposed && trigger.isConnected) trigger.focus(); schedule(); });
  create.onclick = () => run('POST', current ? undefined : templateSelect.value || undefined); stop.onclick = () => run('DELETE');
  quickCreate.onclick = create.onclick; quickStop.onclick = stop.onclick;
  root.refresh = refresh;
  root.start = templateId => { if (!create.disabled && !create.hidden) { open(); return run('POST', templateId); } };
  root.update = (value, available, allowed = canStart) => { canStart = allowed; if (!busy && !disposed) show(value, available); };
  root.setBranchName = (name, workspaceLabel = workspaceName) => { branchName = name; workspaceName = workspaceLabel; identity.textContent = `${workspaceName} / ${branchName}`; updateConfirmation(); };
  root.dispose = () => { disposed = true; clearTimeout(timer); dialog.close(); dialog.remove(); deleteDialog.close(); deleteDialog.remove(); };
  setTemplates(templates);
  if (initial) show(initial.sandbox, initial.enabled); else refresh();
  return root;
}
