export function storageEditor($) {
  let previous = null;
  function show(remote, mode = remote && remote.enabled !== false ? (remote.backend || 's3') : 'local', allowInherit = true, retainSecrets = true) {
    previous = retainSecrets ? remote : null;
    const inherit = $('storage-mode').querySelector('[value=inherit]');
    inherit.hidden = !allowInherit; inherit.disabled = !allowInherit;
    const backend = remote?.backend || 's3';
    $('storage-mode').value = mode;
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
    if (mode === 'inherit') return undefined;
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
    $('s3-bucket').required = mode === 's3';
    const saved = mode === 's3' && $('s3-auth').value === 'saved';
    for (const id of ['s3-access-key', 's3-secret-key', 's3-session-token']) $(id).disabled = !saved;
    $('s3-access-key').required = saved;
    for (const name of ['endpoint','repository','branch','prefix']) $('opencsg-'+name).required = mode === 'opencsg';
    const oldCredentials = previous?.credentials;
    const retainKey = oldCredentials && oldCredentials.access_key_id === $('s3-access-key').value.trim();
    $('s3-secret-key').required = saved && (!retainKey || !!$('s3-session-token').value);
    $('s3-secret-key').placeholder = retainKey ? '留空保留已保存密钥' : '请输入 Secret Access Key';
    const retained = previous?.backend === 'opencsg' && previous.endpoint === $('opencsg-endpoint').value.trim()
      && previous.repository === $('opencsg-repository').value.trim();
    $('opencsg-token').required = mode === 'opencsg' && !retained;
    $('opencsg-token').placeholder = retained ? '留空保留已保存 token' : '请输入仓库访问 token';
  }
  return {show, read, update};
}

export function createSettingsPage({api, element, $}) {
  let current = null;
  let dirty = false;
  let busy = false;
  let generation = 0;
  const gib = 1024 * 1024 * 1024;
  function message(id, text) { $(id).textContent = text || ''; $(id).hidden = !text; }
  function state() {
    $('settings-save').disabled = busy || !dirty;
    $('settings-reload').disabled = busy;
    $('settings-fields').disabled = busy;
    $('settings-state').textContent = busy ? '正在处理，存储切换期间需要传输历史内容…' : dirty ? '有尚未保存的修改' : '当前配置已生效';
    if (storageDirty && !busy) $('settings-state').textContent = '请保存或重新载入后切换配置范围';
    $('settings-reload').textContent = dirty ? '重新载入并放弃修改' : '重新载入';
  }
  function changed() { dirty = true; editor.update(); updateCache(); message('settings-notice', ''); state(); }
  function render(data) {
    current = data; dirty = false;
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
    busy = true; state();
    const request = ++generation;
    try {
      const data = await api('/api/settings');
      if (request !== generation) return;
      if (!dirty || discard) { render(data); message('settings-notice', ''); }
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
    } finally { if (request === generation) { busy = false; state(); } }
  }
  function runtime() {
    const scope = $('storage-scope').value;
    const workspace_remotes = {...current.runtime.workspace_remotes};
    let remote = current.runtime.remote;
    if (storageDirty) {
      const selected = editor.read();
      if (!scope) remote = selected || (remote ? {...remote, enabled:false} : null);
      else if (selected === undefined) delete workspace_remotes[scope];
      else workspace_remotes[scope] = selected;
    }
    const maxBytes = $('cache-max').disabled ? current.runtime.cache.max_bytes : Math.round(Number($('cache-max').value) * gib);
    if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0) throw new Error('本地缓存容量必须大于零，且不能超过浏览器支持的整数范围。');
    return {cache:{max_bytes:maxBytes}, remote, workspace_remotes, collaboration:{enabled:$('settings-collaboration-enabled').checked}, allowed_origins:[...new Set($('allowed-origins').value.split('\n').map(value => value.trim()).filter(Boolean))]};
  }
  const editor = storageEditor($);
  let workspaces = [];
  let storageDirty = false;
  function setWorkspaces(list) {
    workspaces = list;
    const select = $('storage-scope');
    const selected = select.value;
    const names = new Map(workspaces.map(item => [item.id, item.name]));
    for (const id of Object.keys(current?.runtime.workspace_remotes || {})) {
      if (!names.has(id)) names.set(id, id);
    }
    if (selected && dirty && !names.has(selected)) names.set(selected, selected);
    select.replaceChildren(new Option('全部工作区的默认设置', ''));
    for (const [id, name] of names) select.add(new Option(name, id));
    if (names.has(selected)) select.value = selected;
    if (current && select.value !== selected) showScope();
  }
  function showScope() {
    if (!current) return;
    const scope = $('storage-scope').value;
    const remote = scope ? current.runtime.workspace_remotes?.[scope] : current.runtime.remote;
    editor.show(remote, scope && remote === undefined ? 'inherit' : undefined, !!scope);
    storageDirty = false;
    $('storage-scope').disabled = false;
    updateCache();
  }
  function updateCache() {
    const scope = $('storage-scope').value;
    const mode = $('storage-mode').value;
    const selected = mode === 's3' || mode === 'opencsg';
    const active = profile => profile && profile.enabled !== false;
    const anyRemote = selected || (scope && active(current?.runtime.remote)) ||
      Object.entries(current?.runtime.workspace_remotes || {}).some(([id, value]) => id !== scope && active(value));
    $('cache-panel').hidden = !anyRemote;
    $('cache-max').disabled = !anyRemote;
    $('cache-max').required = !!anyRemote;
  }
  $('storage-scope').addEventListener('change', showScope);
  $('settings-form').addEventListener('input', event => {
    if (event.target === $('storage-scope')) return;
    if ($('storage-editor').contains(event.target)) {
      storageDirty = true;
      $('storage-scope').disabled = true;
    }
    changed();
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
    clear() { generation++; current = null; dirty = false; busy = false; storageDirty = false; $('settings-form').reset(); $('settings-form').hidden = true; $('startup-panel').hidden = true; ['settings-error','settings-notice','reload-error'].forEach(id => message(id, '')); state(); },
  };
}
