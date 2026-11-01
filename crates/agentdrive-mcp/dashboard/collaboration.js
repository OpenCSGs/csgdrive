const phaseLabels = {
  disabled:'自动协作未启用', following:'跟随远端更新', writing:'持有写权限',
  waiting_for_handles:'等待句柄关闭', saving:'正在确认同步与权限',
  recovering:'正在保存恢复分支', switching:'正在更新工作目录', failed:'需要处理',
};

export function createCollaborationPanel({api, element, container, changed}) {
  const panel = element('section', 'panel collaboration-panel');
  panel.setAttribute('aria-label', '自动协作');
  const heading = element('div', 'panel-heading');
  heading.append(element('h2', '', '自动协作'));
  const globalState = element('span', 'pill'); globalState.id = 'collaboration-global-state';
  heading.append(globalState);
  const description = element('p', 'collaboration-description');
  const settingsLink = element('a', 'management-link collaboration-settings-link', '前往设置管理全局开关'); settingsLink.href = '/settings';
  const trackingNotice = element('p', 'collaboration-description'); trackingNotice.hidden = true;
  const form = element('form', 'collaboration-form'); form.id = 'collaboration-form';
  const mountLabel = element('label', 'management-field', '本机工作目录');
  const mountInput = element('select'); mountInput.id = 'collaboration-mount';
  mountLabel.append(mountInput);
  const idleLabel = element('label', 'management-field', '修改空闲多久后交还权限');
  const idle = element('input'); idle.type = 'number'; idle.min = '1'; idle.max = '86400'; idle.step = '1'; idle.value = '60'; idle.required = true; idle.id = 'collaboration-idle';
  idleLabel.append(idle, element('small', '', '单位：秒；保留打开的句柄时继续等待。'));
  const timerLabel = element('label', 'collaboration-check');
  const timer = element('input'); timer.type = 'checkbox'; timer.checked = true; timer.id = 'collaboration-timer';
  timerLabel.append(timer, document.createTextNode('按修改空闲时间交还'));
  const eventLabel = element('label', 'collaboration-check');
  const events = element('input'); events.type = 'checkbox'; events.checked = true; events.id = 'collaboration-events';
  eventLabel.append(events, document.createTextNode('接收工作结束事件后交还'));
  const submit = element('button', 'primary', '保存协作设置'); submit.type = 'submit';
  const feedback = element('p', 'collaboration-feedback'); feedback.setAttribute('role', 'status');
  const error = element('p', 'command-error'); error.setAttribute('role', 'alert'); error.hidden = true;
  form.append(mountLabel, timerLabel, idleLabel, eventLabel, submit);
  const state = element('div', 'collaboration-state'); state.setAttribute('aria-live', 'polite');
  const help = element('details', 'collaboration-help');
  help.append(element('summary', '', '接入工作结束事件'),
    element('p', '', '使用 agentdrive integrate 连接 ACP 或 Codex App Server。接入程序登记本次工作，再转发结束事件；普通文件修改自动取得权限。'),
    element('p', '', '勾选事件释放仅启用事件处理。实际通知来源需要完成连接；未接入时可以使用空闲释放。'));
  form.append(element('p', 'collaboration-description', '同一分支的本机挂载共用交还时间、事件设置与写权限。'));
  panel.append(heading, description, settingsLink, trackingNotice, form, feedback, error, state, help); container.append(panel);
  let workspace = null, snapshot = null, statuses = [], generation = 0, dirty = false, running = false, loaded = false, formGeneration = null;
  const selectedMount = () => snapshot?.status.mounts.find(mount => mount.id === mountInput.value);
  const selectedStatus = () => statuses.find(status => status.mounts.includes(mountInput.value));
  const supported = () => snapshot?.status.capabilities.automatic_collaboration === true;
  const globallyEnabled = () => snapshot?.status.capabilities.collaboration_enabled === true;
  function controls() {
    const available = !!selectedMount();
    const mayConfigure = supported() && (!globallyEnabled() || snapshot?.status.capabilities.remote_configured);
    mountInput.disabled = running || !available;
    timer.disabled = events.disabled = running || !available || !mayConfigure;
    idle.disabled = running || !available || !timer.checked || !mayConfigure;
    submit.disabled = running || !available || !loaded || !dirty || !mayConfigure;
  }
  function settings() {
    const value = selectedStatus()?.settings;
    timer.checked = value ? value.idle_timeout_secs != null : true;
    idle.value = value?.idle_timeout_secs ?? 60;
    events.checked = value?.release_on_event ?? true;
    formGeneration = selectedMount()?.generation ?? null;
    dirty = false; controls();
  }
  function line(label, value, className = '') {
    const row = element('div', `collaboration-detail ${className}`);
    row.append(element('span', '', label), element('strong', '', value)); state.append(row);
  }
  function renderStatus() {
    state.replaceChildren();
    const mount = selectedMount(), status = selectedStatus();
    if (!mount) { state.append(element('p', 'panel-empty', '创建本机读写挂载后，可以配置自动协作。')); return; }
    if (!loaded) { state.append(element('p', 'panel-empty', '正在读取协作状态…')); return; }
    const branch = snapshot.branches.find(branch => branch.id === (status?.branch || mount.branch));
    line('当前分支', branch?.name || status?.branch || mount.branch);
    line('工作状态', status?.phase === 'switching' && status.recovery_branch ? '等待切换恢复分支' : phaseLabels[status?.phase || 'disabled'] || status.phase);
    if (!status) return;
    const recovering = status.recovery_branch || status.phase === 'recovering';
    const available = branch?.state === 'available' && ['following','disabled'].includes(status.phase);
    line('写权限', recovering ? '原分支写权限已失效' : status.writer ? status.writer === snapshot.status.location ? '本机持有' : `其他 location ${status.writer.slice(0,8)}` : available ? '可接续' : '等待权限确认');
    line('未关闭句柄', status.open_handles == null ? '未知' : String(status.open_handles));
    const scope = snapshot.status.capabilities.handle_tracking_scope;
    line('句柄统计范围', scope === 'protocol' ? '文件系统协议报告的句柄' : scope === 'native' ? '驱动打开的文件与目录句柄' : '驱动未报告');
    line('已登记的活动任务', String(status.active_executions));
    line('保存状态', `${status.local_saved ? '本机已保存' : '等待本机保存'} · ${status.remote_confirmed ? '远端已确认' : '等待远端确认'}`);
    if (status.recovery_branch) {
      const target = snapshot.branches.find(branch => branch.id === status.recovery_branch);
      const link = element('a', 'management-link', target?.name || status.recovery_branch);
      link.href = `/dashboard?${new URLSearchParams({workspace, branch:status.recovery_branch})}`;
      const row = element('div', 'collaboration-detail'); row.append(element('span', '', '恢复分支'), link); state.append(row);
    }
    if (status.phase === 'waiting_for_handles' || status.phase === 'switching') state.append(element('p', 'collaboration-notice', '请让使用该目录的程序关闭文件和目录句柄。后台会继续完成交还或分支切换。'));
    if (status.error) state.append(element('p', 'command-error', status.error.message));
  }
  async function load() {
    if (running) return;
    const request = ++generation, current = workspace;
    if (!current) return;
    try {
      const result = await api(`/api/workspaces/${encodeURIComponent(current)}/collaboration`);
      if (request !== generation || workspace !== current) return;
      statuses = result; loaded = true; error.hidden = true;
      if (!dirty) settings();
      renderStatus(); controls();
    } catch (failure) {
      if (request !== generation) return;
      error.textContent = failure.message; error.hidden = false; controls();
    }
  }
  function show(data) {
    const next = data?.status.workspace.id || null;
    if (next !== workspace) { clear(); workspace = next; }
    snapshot = data; panel.hidden = !data;
    if (!data) return;
    const selected = mountInput.value;
    const mounts = data.status.mounts.filter(mount => mount.access === 'read_write' && mount.state !== 'released');
    const signature = JSON.stringify(mounts.map(mount => [mount.id,mount.path,mount.branch]));
    if (mountInput.dataset.options !== signature) {
      mountInput.replaceChildren(...mounts.map(mount => new Option(mount.path, mount.id)));
      mountInput.dataset.options = signature;
      mountInput.value = mounts.some(mount => mount.id === selected) ? selected : mounts.find(mount => mount.branch === data.status.selected_branch)?.id || mounts[0]?.id || '';
      if (mountInput.value !== selected) { dirty = false; loaded = false; feedback.textContent = ''; }
    }
    const protocolTracking = data.status.capabilities.handle_tracking_scope === 'protocol';
    globalState.textContent = globallyEnabled() ? '全局已开启' : '全局已关闭';
    description.textContent = !supported() ? '当前驱动不支持自动协作。手动挂载、文件同步和分支操作可以继续使用。'
      : !globallyEnabled() ? '自动协作已在设置中关闭。当前权限由手动管理，已经开始的交还或恢复会继续完成。'
      : data.status.capabilities.remote_configured
      ? '首次修改时自动申请写权限，工作期间自动保存。可跟踪的文件与目录句柄关闭后，按空闲时间或工作结束事件交还权限。'
      : '全局自动协作已开启；配置远程存储后，工作目录可以自动同步和交还权限。';
    trackingNotice.hidden = false;
    trackingNotice.textContent = protocolTracking
      ? '句柄按文件系统协议统计。FUSE-T 的 NFS backend 无法完整报告应用保留的目录引用；后台根据协议可见的句柄判断交还时机。'
      : supported()
      ? '句柄按驱动打开的文件与目录统计。FUSE 的 O_PATH 引用等应用引用可能不触发驱动回调；后台根据驱动可见的句柄判断交还时机。'
      : '当前构建未提供自动协作所需的挂载驱动。全局开关由设置页面管理。';
    if (data.status.capabilities.platform_mount === 'macos-fuse-t') trackingNotice.textContent += ' 接收新版本时自动重新挂载相同目录，切换期间访问可能需要重试。';
    renderStatus(); controls(); load();
  }
  function clear() {
    generation++; workspace = null; snapshot = null; statuses = []; dirty = running = loaded = false; formGeneration = null;
    mountInput.replaceChildren(); delete mountInput.dataset.options; state.replaceChildren(); feedback.textContent = ''; error.hidden = true; panel.hidden = true;
  }
  mountInput.onchange = () => { dirty = false; feedback.textContent = ''; settings(); renderStatus(); };
  form.oninput = () => { dirty = true; feedback.textContent = ''; controls(); };
  form.onsubmit = async event => {
    event.preventDefault(); if (running || !selectedMount() || !form.reportValidity()) return;
    if (!supported()) { error.textContent = '当前挂载驱动不支持自动协作。'; error.hidden = false; return; }
    if (!timer.checked && !events.checked) { error.textContent = '请选择至少一种自动交还方式。'; error.hidden = false; return; }
    const current = workspace, mount = mountInput.value, request = ++generation; let saved = false;
    running = true; error.hidden = true; feedback.textContent = '正在保存设置…'; controls();
    try {
      const result = await api('/api/collaboration', {method:'PUT',headers:{'Content-Type':'application/json'},body:JSON.stringify({
        mount, expected_binding_generation:formGeneration, enabled:globallyEnabled(), idle_timeout_secs:timer.checked ? Number(idle.value) : null, release_on_event:events.checked,
      })});
      if (request !== generation || workspace !== current) return;
      statuses = [...statuses.filter(status => status.branch !== result.branch), result];
      loaded = true; dirty = false; settings(); renderStatus(); feedback.textContent = '协作设置已保存。';
      saved = true;
    } catch (failure) {
      if (request !== generation) return;
      error.textContent = failure.message; error.hidden = false; feedback.textContent = '';
    } finally {
      if (request === generation) { running = false; controls(); }
    }
    if (saved && workspace === current) await changed();
  };
  return {show, clear};
}
