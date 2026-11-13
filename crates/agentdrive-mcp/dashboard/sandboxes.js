import { createSandboxControl } from './sandbox.js';

const labels = {ready:'运行中',creating:'准备中',starting:'启动中',stopping:'停止中',stopped:'已停止',error:'启动失败',idle:'尚未创建'};

export function createSandboxesPage({api, element, container}) {
  let generation = 0, data = null, templateKey = '', workspaceKey = '';
  const cards = new Map();
  const heading = element('div', 'page-heading');
  const intro = element('div');
  intro.append(element('div', 'eyebrow', 'SANDBOXES'), element('h1', '', '沙盒'), element('p', '', '管理所有工作区的 Ubuntu 桌面，在独立环境中操作分支文件。'));
  const create = element('button', 'primary', '创建沙盒'); create.type = 'button'; create.disabled = true;
  heading.append(intro, create);
  const notice = element('p', 'error-banner'); notice.hidden = true; notice.setAttribute('role', 'alert');
  const stats = element('section', 'sandbox-summary'); stats.setAttribute('aria-label', '沙盒统计');
  const counters = ['已创建', '运行中', '启动或停止中', '运行上限'].map(title => {
    const item = element('div'); const value = element('strong', '', '—'); item.append(element('span', '', title), value); stats.append(item); return value;
  });
  const tabs = element('div', 'sandbox-page-tabs'); tabs.setAttribute('role', 'tablist'); tabs.setAttribute('aria-label', '沙盒视图');
  const instancesTab = element('button', '', '沙盒实例'); const templateTab = element('button', '', '模板内容');
  const instances = element('section'); instances.id = 'sandbox-instances'; instances.setAttribute('role', 'tabpanel');
  const template = element('section', 'sandbox-template-list'); template.id = 'sandbox-template'; template.setAttribute('role', 'tabpanel');
  function selectTab(index) {
    instances.hidden = index !== 0; template.hidden = index !== 1;
    [instancesTab, templateTab].forEach((tab, i) => { tab.setAttribute('aria-selected', String(i === index)); tab.tabIndex = i === index ? 0 : -1; });
  }
  [instancesTab, templateTab].forEach((tab, i) => {
    tab.type = 'button'; tab.id = `sandbox-tab-${i}`; tab.setAttribute('role', 'tab'); tab.setAttribute('aria-controls', [instances, template][i].id);
    [instances, template][i].setAttribute('aria-labelledby', tab.id);
    tab.onclick = () => selectTab(i);
    tab.onkeydown = event => {
      if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) {
        event.preventDefault(); const next = event.key === 'Home' ? 0 : event.key === 'End' ? 1 : 1 - i;
        selectTab(next); [instancesTab, templateTab][next].focus();
      }
    };
    tabs.append(tab);
  });
  const filters = element('div', 'sandbox-filters');
  const search = element('input'); search.type = 'search'; search.placeholder = '搜索分支或工作区'; search.setAttribute('aria-label', '搜索沙盒');
  const workspace = element('select'); workspace.setAttribute('aria-label', '筛选工作区');
  const state = element('select'); state.setAttribute('aria-label', '筛选沙盒状态');
  for (const [value, label] of [['existing','全部已创建'],['ready','运行中'],['pending','启动或停止中'],['stopped','已停止'],['error','启动失败'],['idle','可创建的分支']]) {
    const option = element('option', '', label); option.value = value; state.append(option);
  }
  filters.append(search, workspace, state);
  const list = element('div', 'sandbox-catalog');
  const empty = element('div', 'sandbox-empty panel', '正在读取沙盒…'); empty.setAttribute('role', 'status');
  instances.append(filters, list, empty);
  container.append(heading, notice, stats, tabs, instances, template); selectTab(0);
  create.setAttribute('aria-haspopup', 'dialog');
  const createDialog = element('dialog', 'revision-dialog sandbox-create-dialog');
  createDialog.setAttribute('aria-labelledby', 'sandbox-create-title');
  const createHeader = element('div', 'revision-dialog-heading');
  const createTitle = element('h2', '', '创建沙盒'); createTitle.id = 'sandbox-create-title';
  const closeCreate = element('button', 'revision-dialog-close', '×'); closeCreate.type = 'button'; closeCreate.setAttribute('aria-label', '关闭创建沙盒');
  createHeader.append(createTitle, closeCreate);
  const createForm = element('form', 'sandbox-create-form');
  createForm.append(element('p', '', '选择工作区和分支，创建 Ubuntu 桌面。分支文件会自动挂载到 /workspace。'));
  const targetWorkspace = element('select'); targetWorkspace.id = 'sandbox-create-workspace';
  const targetBranch = element('select'); targetBranch.id = 'sandbox-create-branch';
  const targetTemplate = element('select'); targetTemplate.id = 'sandbox-create-template';
  for (const [name, input] of [['工作区', targetWorkspace], ['分支', targetBranch], ['模板', targetTemplate]]) {
    input.setAttribute('aria-label', name);
    const label = element('label', 'management-field'); label.htmlFor = input.id; label.append(element('span', '', name), input); createForm.append(label);
  }
  const templateInfo = element('p', 'sandbox-create-template');
  const createFeedback = element('p', 'sandbox-create-feedback'); createFeedback.setAttribute('role', 'status');
  const branchesLink = element('a', 'management-link', '前往分支管理');
  const createActions = element('div', 'sandbox-actions');
  const cancelCreate = element('button', 'secondary', '取消'); cancelCreate.type = 'button';
  const submitCreate = element('button', 'primary', '创建并启动'); submitCreate.type = 'submit';
  createActions.append(cancelCreate, submitCreate);
  createForm.append(templateInfo, createFeedback, branchesLink, createActions); createDialog.append(createHeader, createForm);
  function options(select, values, preferred, placeholder) {
    const key = JSON.stringify(values);
    if (select.dataset.options !== key) {
      select.replaceChildren();
      for (const [id, label] of values.length ? values : [['', placeholder]]) { const option = element('option', '', label); option.value = id; select.append(option); }
      select.dataset.options = key;
    }
    select.value = values.some(([id]) => id === preferred) ? preferred : values[0]?.[0] || '';
    select.disabled = !values.length;
  }
  function updateCreate() {
    if (!createDialog.open || !data) return;
    const allWorkspaces = [...new Map(data.entries.map(entry => [entry.workspace, entry.workspace_name])).entries()];
    options(targetWorkspace, allWorkspaces, targetWorkspace.value, '暂无工作区');
    const available = data.entries.filter(entry => entry.workspace === targetWorkspace.value && entry.can_start && !entry.sandbox);
    options(targetBranch, available.map(entry => [entry.branch, entry.branch_name]), targetBranch.value, '没有可创建沙盒的分支');
    const active = data.entries.filter(entry => ['ready', 'creating', 'starting', 'stopping'].includes(entry.sandbox?.state)).length;
    const limit = data.template?.max_instances;
    const full = limit != null && active >= limit;
    submitCreate.disabled = !data.enabled || !available.length || full;
    createFeedback.textContent = !data.enabled ? 'Sandbox 运行环境尚未配置。' : !allWorkspaces.length ? '请先创建工作区和分支。' : !available.length ? '此工作区没有可用分支。每个分支对应一个沙盒；可以选择其他工作区，或前往分支管理创建新分支。' : full ? `当前已达到 ${limit} 个运行实例的上限。请先停止一个沙盒，再创建新沙盒。` : '创建后会显示启动步骤，桌面就绪后可以打开桌面或获取 MCP 配置。';
    branchesLink.hidden = !targetWorkspace.value || available.length > 0;
    branchesLink.href = `/branches?workspace=${encodeURIComponent(targetWorkspace.value)}`;
    const templates = data.templates?.length ? data.templates : data.template ? [{id:'ubuntu-desktop',...data.template}] : [];
    options(targetTemplate, templates.map(item => [item.id,item.manifest?.name || item.id]), targetTemplate.value || templates.find(item => item.capabilities?.includes('csgclaw'))?.id, '暂无可用模板');
    const value = templates.find(item => item.id === targetTemplate.value);
    submitCreate.disabled ||= !value;
    templateInfo.textContent = value ? `模板：${value.manifest?.name || 'Ubuntu Desktop'} · ${value.vcpus} vCPU · ${value.memory_gib} GiB 内存 · ${value.disk_gib} GiB 系统磁盘` : '模板：当前配置的 Ubuntu 桌面';
  }
  create.onclick = () => {
    if (!createDialog.isConnected) document.body.append(createDialog);
    createDialog.showModal();
    options(targetWorkspace, [...new Map((data?.entries || []).map(entry => [entry.workspace, entry.workspace_name])).entries()], workspace.value || new URLSearchParams(location.search).get('workspace'), '暂无工作区');
    updateCreate(); targetWorkspace.focus();
  };
  targetWorkspace.onchange = targetTemplate.onchange = updateCreate;
  closeCreate.onclick = cancelCreate.onclick = () => createDialog.close();
  createDialog.addEventListener('close', () => { if (create.isConnected && !container.hidden && !document.querySelector('dialog[open]')) create.focus(); });
  createForm.onsubmit = event => {
    event.preventDefault(); updateCreate();
    if (submitCreate.disabled) return;
    const card = cards.get(`${targetWorkspace.value}/${targetBranch.value}`);
    if (!card || card.entry.sandbox || !card.entry.can_start) return;
    createDialog.close(); selectTab(0); state.value = 'existing'; search.value = ''; workspace.value = targetWorkspace.value;
    card.entry.sandbox = {state:'creating', startup_step:'prepare_branch'}; filter();
    card.control.start(targetTemplate.value);
  };
  search.oninput = filter; workspace.onchange = filter; state.onchange = filter;
  function filter() {
    let count = 0;
    const query = search.value.trim().toLocaleLowerCase();
    for (const {node, entry} of cards.values()) {
      const current = entry.sandbox?.state || 'idle';
      const matches = state.value === 'existing' ? !!entry.sandbox : state.value === 'pending' ? ['creating','starting','stopping'].includes(current) : state.value === 'idle' ? !entry.sandbox && entry.can_start : current === state.value;
      node.hidden = !matches || (!!workspace.value && workspace.value !== entry.workspace) || !`${entry.workspace_name} ${entry.branch_name} ${entry.branch}`.toLocaleLowerCase().includes(query);
      if (!node.hidden) count++;
    }
    empty.hidden = count > 0;
    empty.textContent = !data ? '正在读取沙盒…' : !data.enabled ? 'Sandbox 运行环境尚未配置。配置完成后即可创建沙盒。' : state.value === 'idle' ? '没有符合条件的分支。可以在分支管理中创建分支，或启动已停止的沙盒。' : '没有符合条件的沙盒。点击「创建沙盒」选择工作区分支。';
  }
  function renderTemplate() {
    const values = data.templates?.length ? data.templates : data.template ? [{id:'ubuntu-desktop',...data.template}] : [];
    const key = JSON.stringify([data.enabled, values]); if (templateKey === key) return; templateKey = key;
    template.replaceChildren();
    if (!values.length) { template.append(element('p', 'sandbox-empty panel', '尚未配置模板')); return; }
    for (const value of values) {
      const card = element('article', 'sandbox-template panel');
      const header = element('div', 'sandbox-template-heading');
      const title = element('div'); title.append(element('div', 'sandbox-eyebrow', value.id),element('h2', '', value.manifest?.name || value.id));
      const use = element('button', 'primary', '使用此模板创建'); use.type = 'button'; use.disabled = !data.enabled;
      use.onclick = () => { create.click(); targetTemplate.value = value.id; updateCreate(); };
      header.append(element('div', 'sandbox-template-icon', '▧'),title); card.append(header);
      const capabilities = element('div', 'sandbox-capabilities');
      const labels = {desktop:'桌面',browser:'浏览器',mcp:'统一 MCP',openclaw:'OpenClaw',csgclaw:'CSGClaw',environment:'开发环境'};
      for (const capability of value.capabilities || ['desktop','browser','mcp']) capabilities.append(element('span','pill',labels[capability] || capability));
      card.append(capabilities);
      const overview = element('dl', 'sandbox-template-overview');
      const fields = element('dl', 'sandbox-template-fields');
      function field(name, content, target = fields) { const group = element('div'); group.append(element('dt','',name),element('dd','',content)); target.append(group); }
      const manifest = value.manifest;
      if (manifest) {
        field('操作系统',manifest.os,overview); field('桌面环境',manifest.desktop,overview); field('默认浏览器',manifest.browser,overview);
        field('语言环境',manifest.locale); field('字体',manifest.fonts.join('\n')); field('桌面工具',manifest.tools.join('\n'));
        field('分支目录',manifest.workspace_path); field('清单生成时间',new Date(manifest.built_at).toLocaleString('zh-CN',{hour12:false}));
      }
      field('每个实例的资源',`${value.vcpus} vCPU · ${value.memory_gib} GiB 内存 · ${value.disk_gib} GiB 系统磁盘`,overview);
      field('模板版本',value.version || 'local');
      field('并发与网络',`最多 ${value.max_instances} 个运行实例 · ${value.network_slots} 个已配置网络接口`);
      const details = element('button', 'secondary', '查看详情'); details.type = 'button'; details.setAttribute('aria-haspopup', 'dialog');
      const dialog = element('dialog', 'revision-dialog sandbox-template-dialog');
      dialog.setAttribute('aria-label', `${manifest?.name || value.id} · 模板详情`);
      const dialogHeader = element('div', 'revision-dialog-heading');
      const dialogTitle = element('div'); dialogTitle.append(element('div', 'sandbox-eyebrow', '模板详情'),element('h2', '', manifest?.name || value.id));
      const close = element('button', 'revision-dialog-close', '×'); close.type = 'button'; close.setAttribute('aria-label', '关闭模板详情');
      dialogHeader.append(dialogTitle,close);
      const body = element('div', 'sandbox-template-dialog-body');
      const configuration = element('dl', 'sandbox-template-fields');
      configuration.append(...Array.from(overview.children, group => group.cloneNode(true)),...fields.children);
      body.append(configuration,element('p','sandbox-template-note','分支文件自动挂载到 /workspace。已有沙盒继续使用创建时的系统磁盘。'));
      dialog.append(dialogHeader,body);
      details.onclick = () => dialog.showModal();
      close.onclick = () => dialog.close();
      dialog.addEventListener('close', () => { if (details.isConnected && !template.hidden) details.focus(); });
      const actions = element('div', 'sandbox-template-actions'); actions.append(details,use);
      card.append(overview,actions,dialog); template.append(card);
    }
  }
  function render() {
    notice.hidden = !data.errors.length; notice.textContent = data.errors.join('；');
    create.disabled = !data.enabled;
    const existing = data.entries.filter(entry => entry.sandbox);
    counters[0].textContent = existing.length;
    counters[1].textContent = existing.filter(entry => entry.sandbox.state === 'ready').length;
    counters[2].textContent = existing.filter(entry => ['creating','starting','stopping'].includes(entry.sandbox.state)).length;
    counters[3].textContent = data.template?.max_instances ?? '—';
    const workspaces = [...new Map(data.entries.map(entry => [entry.workspace, entry.workspace_name])).entries()];
    const key = JSON.stringify(workspaces);
    if (workspaceKey !== key) {
      const selected = workspace.value; workspace.replaceChildren();
      for (const [id, name] of [['', '所有工作区'], ...workspaces]) { const option = element('option', '', name); option.value = id; workspace.append(option); }
      workspace.value = workspaces.some(([id]) => id === selected) ? selected : ''; workspaceKey = key;
    }
    const retained = new Set();
    for (const entry of data.entries) {
      const id = `${entry.workspace}/${entry.branch}`; retained.add(id);
      let card = cards.get(id);
      if (!card) {
        const node = element('article', 'sandbox-card panel'); node.dataset.branch = entry.branch;
        const top = element('div', 'sandbox-card-heading'); const title = element('h3'); const badge = element('span', 'sandbox-badge'); top.append(title, badge);
        const identity = element('p', 'sandbox-card-identity'); const message = element('p', 'sandbox-card-message');
        const control = createSandboxControl({api, element, workspace:entry.workspace, branch:entry.branch, branchName:entry.branch_name, workspaceName:entry.workspace_name, inline:true, templates:data.templates || [], initial:{sandbox:entry.sandbox,enabled:data.enabled}, canStart:entry.can_start, onDeleted:() => {
          generation++;
          const current = data?.entries.find(item => item.workspace === entry.workspace && item.branch === entry.branch);
          if (current) { current.sandbox = null; render(); }
          load();
        }});
        const footer = element('div', 'sandbox-card-footer'); const branchLink = element('a', 'management-link', '查看分支');
        branchLink.href = `/branches?workspace=${encodeURIComponent(entry.workspace)}&branch=${encodeURIComponent(entry.branch)}`;
        footer.append(element('code', '', '/workspace'), branchLink);
        node.append(top, identity, message, control, footer); list.append(node);
        card = {node, title, badge, identity, message, control}; cards.set(id, card);
      }
      card.entry = entry; card.title.textContent = entry.branch_name; card.identity.textContent = `${entry.workspace_name} · ${entry.branch.slice(0, 8)}${entry.sandbox?.template_id ? ' · ' + ((data.templates || []).find(item => item.id === entry.sandbox.template_id)?.manifest?.name || entry.sandbox.template_id) : ''}`;
      const state = entry.sandbox?.state || 'idle'; card.badge.textContent = labels[state] || state; card.badge.dataset.state = state;
      card.message.textContent = entry.sandbox?.message || (state === 'idle' ? '创建 Ubuntu 桌面，自动挂载此分支文件。' : state === 'stopped' ? '系统磁盘已保留，可以继续启动。' : '分支文件自动挂载到 /workspace。');
      card.control.setBranchName(entry.branch_name, entry.workspace_name); card.control.update(entry.sandbox, data.enabled, entry.can_start);
    }
    for (const [id, card] of cards) if (!retained.has(id)) { card.control.dispose(); card.node.remove(); cards.delete(id); }
    filter(); renderTemplate(data.template); updateCreate();
  }
  async function load() {
    const request = ++generation;
    try { const result = await api('/api/sandboxes'); if (request === generation) { data = result; render(); } }
    catch (error) { if (request === generation) { notice.hidden = false; notice.textContent = `沙盒数据刷新失败：${error.message}`; if (!data) empty.textContent = '暂时无法读取沙盒，请点击页面右上方刷新按钮重试。'; } }
  }
  function clear() { generation++; createDialog.close(); createDialog.remove(); for (const dialog of template.querySelectorAll('dialog')) dialog.close(); template.replaceChildren(); for (const card of cards.values()) card.control.dispose(); cards.clear(); list.replaceChildren(); data = null; templateKey = ''; workspaceKey = ''; }
  return {load, clear};
}
