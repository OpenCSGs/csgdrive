const phaseLabels = {
  disabled:'自动协作未启用', following:'跟随远端更新', writing:'持有写权限',
  saving:'正在确认同步与权限', recovering:'正在保存恢复分支', switching:'正在更新工作目录', failed:'需要处理',
};

export function createCollaborationPanel({api, element, container}) {
  const panel = element('section', 'panel collaboration-panel');
  panel.setAttribute('aria-label', '自动协作');
  const heading = element('div', 'panel-heading');
  const globalState = element('span', 'pill'); globalState.id = 'collaboration-global-state';
  heading.append(element('h2', '', '自动协作'), globalState);
  const description = element('p', 'collaboration-description');
  const settingsLink = element('a', 'management-link collaboration-settings-link', '前往设置管理全局开关'); settingsLink.href = '/settings';
  const mountLabel = element('label', 'management-field collaboration-mount', '本机工作目录');
  const mountInput = element('select'); mountInput.id = 'collaboration-mount'; mountLabel.append(mountInput);
  const error = element('p', 'command-error'); error.setAttribute('role', 'alert'); error.hidden = true;
  const state = element('div', 'collaboration-state'); state.setAttribute('aria-live', 'polite');
  const help = element('details', 'collaboration-help');
  help.append(element('summary', '', '接入工作结束回调'),
    element('p', '', '使用 agentdrive integrate 接入 ACP 或 Codex App Server。自定义接入程序通过 POST /api/collaboration/events 登记 started，工作结束时使用相同的 mount、source、thread_id 和 turn_id 发送终止状态。'));
  panel.append(heading, description, settingsLink, mountLabel, error, state, help); container.append(panel);
  let snapshot = null, statuses = [], loaded = false, request = null;
  function line(label, value) {
    const row = element('div', 'collaboration-detail');
    row.append(element('span', '', label), element('strong', '', value)); state.append(row);
  }
  function renderStatus() {
    state.replaceChildren();
    const mount = snapshot?.status.mounts.find(item => item.id === mountInput.value);
    if (!mount) { state.append(element('p', 'panel-empty', '创建本机读写挂载后，可在这里查看交接状态。')); return; }
    if (!loaded) { state.append(element('p', 'panel-empty', '正在读取协作状态…')); return; }
    const status = statuses.find(item => item.mounts.includes(mount.id));
    const branch = snapshot.branches.find(item => item.id === (status?.branch || mount.branch));
    line('当前分支', branch?.name || mount.branch);
    line('工作状态', status?.phase === 'switching' && status.recovery_branch ? '等待切换恢复分支' : status ? phaseLabels[status.phase] || status.phase : phaseLabels.disabled);
    if (!status) return;
    const recovering = status.recovery_branch || status.phase === 'recovering';
    const available = branch?.state === 'available' && ['following','disabled'].includes(status.phase);
    line('写权限', recovering ? '原分支写权限已失效' : status.writer ? status.writer === snapshot.status.location ? '本机持有' : `其他 location ${status.writer.slice(0,8)}` : available ? '可接续' : '等待权限确认');
    line('已登记的活动任务', String(status.active_executions));
    line('保存状态', `${status.local_saved ? '本机已保存' : '等待本机保存'} · ${status.remote_confirmed ? '远端已确认' : '等待远端确认'}`);
    if (status.recovery_branch) {
      const target = snapshot.branches.find(item => item.id === status.recovery_branch);
      const link = element('a', 'management-link', target?.name || status.recovery_branch);
      link.href = `/dashboard?${new URLSearchParams({workspace:snapshot.status.workspace.id, branch:status.recovery_branch})}`;
      const row = element('div', 'collaboration-detail'); row.append(element('span', '', '恢复分支'), link); state.append(row);
    }
    if (status.error) state.append(element('p', 'command-error', status.error.message));
  }
  async function load(workspace) {
    if (request) return;
    const pending = api(`/api/workspaces/${encodeURIComponent(workspace)}/collaboration`);
    request = pending;
    try {
      const result = await pending;
      if (request !== pending) return;
      statuses = result; loaded = true; error.hidden = true; renderStatus();
    } catch (failure) {
      if (request === pending) { error.textContent = failure.message; error.hidden = false; }
    } finally {
      if (request === pending) request = null;
    }
  }
  function clear() {
    snapshot = null; request = null; statuses = []; loaded = false;
    mountInput.replaceChildren(); delete mountInput.dataset.options;
    state.replaceChildren(); error.hidden = true; panel.hidden = true;
  }
  function show(data) {
    if (data?.status.workspace.id !== snapshot?.status.workspace.id) clear();
    snapshot = data; panel.hidden = !data;
    if (!data) return;
    const mounts = data.status.mounts.filter(mount => mount.access === 'read_write' && mount.state !== 'released');
    const signature = JSON.stringify(mounts.map(mount => [mount.id,mount.path,mount.branch]));
    if (mountInput.dataset.options !== signature) {
      const selected = mountInput.value;
      mountInput.replaceChildren(...mounts.map(mount => new Option(mount.path, mount.id)));
      mountInput.dataset.options = signature;
      mountInput.value = mounts.some(mount => mount.id === selected) ? selected : mounts.find(mount => mount.branch === data.status.selected_branch)?.id || mounts[0]?.id || '';
    }
    mountInput.disabled = !mounts.length;
    const capabilities = data.status.capabilities;
    globalState.textContent = capabilities.collaboration_enabled ? '全局已开启' : '全局已关闭';
    description.textContent = !capabilities.automatic_collaboration ? '当前驱动不支持自动协作。'
      : !capabilities.collaboration_enabled ? '自动协作已在设置中关闭。'
      : !capabilities.remote_configured ? '配置远程存储后，工作目录可以自动同步和交还写权限。'
      : '每次 turn 结束并发送回调后，AgentDrive 提交本次修改。当前分支的全部 turn 结束且远端确认保存后，交还写权限。';
    renderStatus(); load(data.status.workspace.id);
  }
  mountInput.onchange = renderStatus;
  return {show, clear};
}
