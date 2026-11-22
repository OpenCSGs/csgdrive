export function createSettingsPage({api, element, $}) {
  let current = null;
  let workspace = '';
  let workspaces = [];
  let generation = 0;
  function message(id, text) {
    $(id).textContent = text;
    $(id).hidden = !text;
  }
  function details(id, rows) {
    $(id).replaceChildren(...rows.flatMap(([label, value]) =>
      [element('dt', '', label), element('dd', '', value)]));
  }
  function render(data) {
    current = data;
    const {runtime, startup} = data;
    $('collaboration-state').textContent = (runtime.collaboration?.enabled ?? true) ? '已启用' : '已关闭';
    $('cache-capacity').textContent = `${runtime.cache.max_bytes / 1024 ** 3} GiB`;
    $('allowed-origins').textContent = runtime.allowed_origins.join('\n') || '未配置';
    details('startup-values', [
      ['配置文件', data.config_path], ['监听地址', startup.listen], ['存储目录', startup.data_dir],
      ['挂载目录', startup.mount_roots.join('\n') || '未配置'], ['导入导出目录', (startup.directory_roots || []).join('\n') || '未配置'],
    ]);
    setWorkspaces(workspaces);
    showScope();
    $('settings-version').textContent = `配置版本 ${data.revision.slice(0, 12)}`;
    $('settings-values').hidden = false;
  }
  async function load() {
    const request = ++generation;
    try {
      const data = await api('/api/settings');
      if (request !== generation || location.pathname !== '/settings') return;
      if (current?.revision !== data.revision) render(data);
      message('reload-error', data.reload_error ? `配置文件提示：${data.reload_error}` : '');
      message('settings-error', '');
      $('updated').textContent = `${new Date().toLocaleTimeString('zh-CN', {hour12:false})} 已刷新`;
      $('connection').textContent = '已连接本地服务';
    } catch (error) {
      if (request !== generation || location.pathname !== '/settings') return;
      message('settings-error', error.message);
      $('updated').textContent = '刷新失败';
      $('connection').textContent = '连接需要检查';
    }
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
    details('storage-details', rows);
  }
  $('storage-scope').addEventListener('change', () => {
    workspace = $('storage-scope').value;
    const url = new URL(location.href);
    url.searchParams.set('workspace', workspace);
    history.replaceState(null, '', url.pathname + url.search);
    showScope();
  });
  return {load, setWorkspaces};
}
