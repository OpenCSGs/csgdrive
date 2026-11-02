export function createRelationshipDetail({element}) {
  const dialog = element('dialog', 'revision-dialog relationship-dialog');
  dialog.id = 'relationship-dialog'; dialog.setAttribute('aria-labelledby', 'relationship-dialog-title');
  const heading = element('div', 'revision-dialog-heading'), copy = element('div');
  const title = element('h2', '', '分支与挂载详情'); title.id = 'relationship-dialog-title';
  const subtitle = element('p', 'revision-dialog-subtitle'); copy.append(title, subtitle);
  const close = element('button', 'revision-dialog-close', '×'); close.type = 'button';
  close.autofocus = true; close.setAttribute('aria-label', '关闭关系详情'); heading.append(copy, close);
  const content = element('div', 'relationship-detail'); dialog.append(heading, content); document.body.append(dialog);
  let context = null, signature = null;
  const triggerId = entry => `relationship-detail-${entry.branch.id}-${entry.mount?.id || 'unmounted'}`;

  function clear(restoreFocus = false) {
    const trigger = context && document.getElementById(triggerId(context.entry));
    context = null; signature = null; content.replaceChildren();
    if (dialog.open) dialog.close();
    if (restoreFocus) trigger?.focus({preventScroll:true});
  }
  function section(label, fields) {
    const section = element('section'), grid = element('dl', 'detail-grid');
    for (const [name, value] of fields) {
      const field = element('div', 'detail-field');
      field.append(element('dt', '', name), element('dd', '', value ?? '—')); grid.append(field);
    }
    section.append(element('h3', 'revision-section-title', label), grid); content.append(section);
  }
  function render() {
    const {branch, mount, mount_location} = context.entry, owner = branch.ownership, origin = branch.origin;
    subtitle.textContent = branch.name || branch.id;
    content.replaceChildren();
    const sources = {local_authority:'本机管理写入',remote_reference:'远端观测记录',local_record:'本机保留记录'};
    const record = [['分支 ID', branch.id], ['写入所属 location', owner.location], ['记录来源', sources[owner.source] || owner.source],
      ['authority_epoch', owner.authority_epoch], ['generation', owner.generation]];
    if (owner.source === 'remote_reference') record.push(
      ['观测时间', owner.observed_ns == null ? '未知' : new Date(owner.observed_ns / 1e6).toLocaleString('zh-CN', {hour12:false})],
      ['存储版本', owner.storage_version ?? '未知']);
    section('分支记录', record);
    if (mount) section('挂载信息', [
      ['挂载 ID', mount.id], ['挂载目录', mount.path], ['挂载 location', mount_location], ['使用者', mount.principal],
      ['访问方式', mount.access === 'read_only' ? '只读' : '读写'], ['generation', mount.generation], ['固定版本', mount.revision ?? '跟随分支'],
    ]);
    else section('挂载信息', [['本机挂载', '本机暂无挂载']]);
    if (origin.status === 'restricted') section('创建来源', [['来源信息', '无权限']]);
    else {
      const fields = [['来源分支', origin.source_branch ? origin.source_name || origin.source_branch : origin.workspace_root ? '工作区初始版本' : '未记录'],
        ['来源分支 ID', origin.source_branch], ['来源版本', origin.base_revision], ['创建时的内容对象', origin.captured_working_root?.id ?? '未记录']];
      if (origin.status === 'unavailable') fields.push(['来源记录', '记录不完整']);
      section('创建来源', fields);
    }
  }
  function show(workspace, entry) {
    clear(); context = {workspace, entry}; signature = JSON.stringify(entry); render();
    dialog.showModal(); dialog.scrollTop = 0;
  }
  function bind(button, workspace, entry) {
    button.id = triggerId(entry); button.type = 'button';
    button.setAttribute('aria-haspopup', 'dialog'); button.setAttribute('aria-controls', dialog.id);
    button.onclick = () => show(workspace, entry);
  }
  function update(workspace, entries) {
    if (!context) return;
    const entry = entries.find(entry => triggerId(entry) === triggerId(context.entry));
    if (workspace !== context.workspace || !entry) { clear(); return; }
    const next = JSON.stringify(entry);
    if (signature !== next) {
      const scroll = dialog.scrollTop;
      context.entry = entry; signature = next; render(); dialog.scrollTop = scroll;
    }
  }
  close.onclick = () => clear(true);
  dialog.addEventListener('cancel', event => { event.preventDefault(); clear(true); });
  let backdropPressed = false;
  const outside = event => {
    const rect = dialog.getBoundingClientRect();
    return event.target === dialog && (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom);
  };
  dialog.addEventListener('pointerdown', event => { backdropPressed = outside(event); });
  dialog.addEventListener('click', event => { if (backdropPressed && outside(event)) clear(true); backdropPressed = false; });
  return {bind, update, clear};
}
