export function createSettingsPage({api, element, $}) {
  let current = null;
  let globalDirty = false;
  let workspace = '';
  let workspaces = [];
  let busy = false;
  let generation = 0;
  const gib = 1024 * 1024 * 1024;
  function message(id, text) { $(id).textContent = text || ''; $(id).hidden = !text; }
  function state() {
    const dirty = globalDirty;
    $('settings-save').disabled = busy || !dirty;
    $('settings-reload').disabled = busy;
    $('settings-fields').disabled = busy;
    $('settings-state').textContent = busy ? '正在处理…' : dirty ? '有尚未保存的修改' : '当前配置已生效';
    $('settings-reload').textContent = dirty ? '重新载入并放弃修改' : '重新载入';
  }
  function render(data) {
    current = data; globalDirty = false;
    $('settings-form').hidden = false; $('startup-panel').hidden = false;
    $('cache-max').value = data.runtime.cache.max_bytes / gib;
    $('settings-collaboration-enabled').checked = data.runtime.collaboration?.enabled ?? true;
    $('allowed-origins').value = data.runtime.allowed_origins.join('\n');
    setWorkspaces(workspaces);
    showScope();
    const values = $('startup-values'); values.replaceChildren();
    const startup = data.startup;
    const fields = [
      ['配置文件', data.config_path], ['监听地址', startup.listen], ['存储目录', startup.data_dir],
      ['挂载目录', startup.mount_roots.join('\n') || '未配置'], ['导入导出目录', (startup.directory_roots || []).join('\n') || '未配置'],
    ];
    for (const [label, value] of fields) values.append(element('dt', '', label), element('dd', '', value));
    $('settings-version').textContent = `配置版本 ${data.revision.slice(0, 12)}`;
    message('reload-error', data.reload_error ? `配置文件提示：${data.reload_error}` : '');
    state();
  }
  async function load(discard = false) {
    if (busy) return;
    const foreground = discard || !current;
    if (foreground) { busy = true; state(); }
    const request = ++generation;
    try {
      const data = await api('/api/settings');
      if (request !== generation) return;
      if (discard || (!globalDirty && current?.revision !== data.revision)) { render(data); message('settings-notice', ''); }
      else {
        message('reload-error', data.reload_error ? `配置文件提示：${data.reload_error}` : '');
        if (current?.revision !== data.revision) message('settings-notice', '配置已被其他操作更新。当前编辑内容已保留，请重新载入后修改。');
      }
      message('settings-error', '');
      $('updated').textContent = `${new Date().toLocaleTimeString('zh-CN', {hour12:false})} 已刷新`;
      $('connection').textContent = '已连接本地服务';
    } catch (error) {
      if (request !== generation) return;
      message('settings-error', error.message);
      $('updated').textContent = '刷新失败'; $('connection').textContent = '连接需要检查';
    } finally { if (foreground && request === generation) { busy = false; state(); } }
  }
  function runtime() {
    const maxBytes = $('cache-max').disabled ? current.runtime.cache.max_bytes : Math.round(Number($('cache-max').value) * gib);
    if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0) throw new Error('本地缓存容量必须大于零，且不能超过浏览器支持的整数范围。');
    return {...current.runtime, cache:{max_bytes:maxBytes}, collaboration:{enabled:$('settings-collaboration-enabled').checked}, allowed_origins:[...new Set($('allowed-origins').value.split('\n').map(value => value.trim()).filter(Boolean))]};
  }
  function setWorkspaces(list, preferred) {
    workspaces = list;
    const names = new Map(list.map(item => [item.id, item.name]));
    for (const id of Object.keys(current?.runtime.workspace_remotes || {})) {
      if (!names.has(id)) names.set(id, id);
    }
    const select = $('storage-scope');
    const entries = [...names];
    if (entries.length !== select.options.length || entries.some(([id, name], i) =>
      select.options[i].value !== id || select.options[i].text !== name)) {
      select.replaceChildren(...entries.map(([id, name]) => new Option(name, id)));
    }
    const requested = new URLSearchParams(location.search).get('workspace');
    const next = [requested, workspace, preferred].find(id => names.has(id)) || entries[0]?.[0] || '';
    select.value = next;
    $('storage-selector').hidden = !next;
    $('storage-details').hidden = !next;
    $('storage-empty').hidden = !!next;
    if (workspace !== next) {
      workspace = next;
      showScope();
    }
  }
  function showScope() {
    if (!current) return;
    const remote = current.runtime.workspace_remotes?.[workspace];
    const rows = [['存储模式', '仅本地存储']];
    if (remote && remote.enabled !== false) {
      if (remote.backend === 'opencsg') {
        rows[0][1] = 'OpenCSG Codes';
        rows.push(['OpenCSG 域名', remote.endpoint], ['Codes 仓库', remote.repository],
          ['存储分支', remote.branch], ['存储目录', remote.prefix], ['XNet', remote.xnet ? '已启用' : '未启用']);
      } else {
        rows[0][1] = 'S3 兼容存储（含 OSS）';
        rows.push(['Endpoint', remote.endpoint || '服务环境或 AWS 默认地址'], ['Bucket', remote.bucket],
          ['Region', remote.region], ['Prefix', remote.prefix]);
      }
    }
    if ((!remote || remote.enabled === false) && workspace) rows.push(['存储目录', current.startup.data_dir]);
    $('storage-details').replaceChildren(...rows.flatMap(([label, value]) =>
      [element('dt', '', label), element('dd', '', value)]));
    const anyRemote = Object.values(current.runtime.workspace_remotes || {}).some(value => value && value.enabled !== false);
    $('cache-panel').hidden = !anyRemote;
    $('cache-max').disabled = !anyRemote;
    $('cache-max').required = !!anyRemote;
  }
  $('storage-scope').addEventListener('change', () => {
    workspace = $('storage-scope').value;
    const url = new URL(location.href);
    url.searchParams.set('workspace', workspace);
    history.replaceState(null, '', url.pathname + url.search);
    showScope();
    message('settings-notice', '');
  });
  $('settings-form').addEventListener('input', event => {
    if (event.target === $('storage-scope')) return;
    globalDirty = true;
    message('settings-notice', '');
    state();
  });
  $('settings-reload').addEventListener('click', () => load(true));
  $('settings-form').addEventListener('submit', async event => {
    event.preventDefault();
    if (busy || !current) return;
    const request = ++generation;
    try {
      const update = {revision:current.revision, runtime:runtime()};
      busy = true; state(); message('settings-error', '');
      const data = await api('/api/settings', {method:'PUT', headers:{'Content-Type':'application/json'}, body:JSON.stringify(update)});
      if (request !== generation) return;
      render(data); message('settings-notice', '配置已保存并生效。');
    } catch (error) {
      if (request === generation) message('settings-error', error.message);
    } finally { if (request === generation) { busy = false; state(); } }
  });
  return {
    load, setWorkspaces,
    clear() { generation++; current = null; globalDirty = false; busy = false; workspace = ''; $('settings-form').reset(); $('settings-form').hidden = true; $('startup-panel').hidden = true; ['settings-error','settings-notice','reload-error'].forEach(id => message(id, '')); state(); },
  };
}
