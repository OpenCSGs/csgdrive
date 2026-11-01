import {createMergeDetail} from './merge-detail.js';
import {mergeStatuses, short, timestamp, branchName, mergeResult, syncLabel, stateBadge, revisionLabel} from './merge-presentation.js';

const filterKeys = ['branch','source_branch','target_branch','status','location','from_ns','to_ns'];
const queryKey = key => `merge_${key}`;
const endpoint = workspace => `/api/workspaces/${encodeURIComponent(workspace)}/merges`;

export function createMergesView({api, element, container, viewRevision}) {
  const heading = element('div', 'merge-heading');
  const title = element('h2', '', '合并记录'), count = element('span', 'count-badge', '0');
  title.append(count);
  const intro = element('div'); intro.append(title, element('p', '', '查看来源、目标、候选更新和合并结果。'));
  heading.append(intro);
  const form = element('form', 'merge-filters'); form.setAttribute('aria-label', '筛选合并记录');
  const primary = element('div', 'merge-filter-main'), extra = element('details', 'merge-filter-extra');
  const summary = element('summary', '', '更多筛选'), fields = element('div', 'merge-filter-fields');
  extra.append(summary, fields);
  const inputs = {};
  function field(key, label, host, type = 'select') {
    const wrapper = element('label', 'management-field', label), input = element(type === 'select' ? 'select' : 'input');
    input.id = `merges-filter-${key}`; input.name = key;
    if (type !== 'select') input.type = type;
    input.setAttribute('aria-label', label); wrapper.append(input); host.append(wrapper); inputs[key] = input;
    return input;
  }
  field('branch', '相关分支', primary);
  field('status', '合并状态', primary).append(new Option('全部状态', ''), ...Object.entries(mergeStatuses).map(([value,text]) => new Option(text,value)));
  field('source_branch', '来源分支', fields); field('target_branch', '目标分支', fields); field('location', '执行节点', fields);
  field('from_ns', '开始时间', fields, 'datetime-local'); field('to_ns', '结束时间', fields, 'datetime-local');
  const actions = element('div', 'merge-filter-actions');
  const submit = element('button', 'secondary', '查询'); submit.type = 'submit';
  const selected = element('button', 'secondary', '当前分支'); selected.type = 'button';
  const reset = element('button', 'secondary', '重置筛选'); reset.type = 'button';
  actions.append(submit, selected, reset); primary.append(actions); form.append(primary, extra);
  const revisionFilter = element('div', 'merge-revision-filter'); revisionFilter.hidden = true;
  const status = element('p', 'merge-list-status'); status.id = 'merges-status'; status.setAttribute('role','status');
  const notice = element('button', 'secondary', '记录已更新，查看更新'); notice.type = 'button'; notice.id = 'merges-reload'; notice.hidden = true;
  const table = element('table', 'merge-table'), head = element('thead'), headers = element('tr');
  for (const text of ['来源 → 目标','合并状态','远端保存','调用身份 / 节点','发起时间','结果版本']) headers.append(element('th','',text));
  head.append(headers);
  const body = element('tbody'); body.id = 'merge-list'; table.append(head, body);
  const empty = element('p', 'merge-empty', '暂无合并记录。'); empty.hidden = true;
  const more = element('button', 'secondary', '加载更多合并记录'); more.type = 'button'; more.id = 'merges-more'; more.hidden = true;
  container.append(heading, form, revisionFilter, status, notice, table, empty, more);
  let context = null, entries = [], cursors = [null], next = null, total = 0, generation = 0, controller = null, busy = false, filters = {};
  const detail = createMergeDetail({api, element, viewRevision, closed:() => {
    const url = new URL(location.href); url.searchParams.delete('merge'); history.replaceState(null,'',url.pathname+url.search);
  }});
  function selectOptions(input, options, placeholder) {
    const selected = input.value;
    const values = [['',placeholder], ...options];
    if (selected && !values.some(([value]) => value === selected)) values.push([selected,short(selected)]);
    const signature = JSON.stringify(values);
    if (input.dataset.options === signature) return;
    input.replaceChildren(...values.map(([value,label]) => new Option(label,value))); input.value = selected; input.dataset.options = signature;
  }
  function renderFilters() {
    const branches = (context?.snapshot.branches || []).map(branch => [branch.id,branch.name]);
    for (const key of ['branch','source_branch','target_branch']) selectOptions(inputs[key], branches, '全部分支');
    const locations = new Set((context?.snapshot.branches || []).map(branch => branch.owner).filter(Boolean));
    for (const entry of entries) if (entry.origin_location) locations.add(entry.origin_location);
    const local = context?.snapshot.status.location;
    selectOptions(inputs.location, [...locations].map(value => [value,value === local ? `本机 · ${short(value)}` : short(value)]), '全部节点');
    selected.disabled = !context?.selectedBranch;
    revisionFilter.replaceChildren(); revisionFilter.hidden = !filters.revision;
    if (filters.revision) {
      const remove = element('button', 'merge-clear-filter', '清除版本筛选'); remove.type = 'button';
      remove.onclick = () => { delete filters.revision; applyFilters(); };
      revisionFilter.append(revisionLabel(element, filters.revision, '关联版本 '), remove);
    }
  }
  function readFilters() {
    const params = new URLSearchParams(location.search); filters = {};
    for (const key of filterKeys) {
      const value = params.get(queryKey(key)) || '';
      if (value) filters[key] = value;
      if (key.endsWith('_ns')) {
        if (!value) inputs[key].value = '';
        else {
          const date = new Date(Number(value) / 1e6);
          inputs[key].value = Number.isFinite(date.getTime()) ? new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0,16) : '';
        }
      } else {
        if (value && ![...inputs[key].options].some(option => option.value === value)) inputs[key].append(new Option(short(value), value));
        inputs[key].value = value;
      }
    }
    const revision = params.get('revision'); if (revision) filters.revision = revision;
    extra.open = ['source_branch','target_branch','location','from_ns','to_ns'].some(key => filters[key]);
  }
  function clear(resetFilters = false) {
    generation++; controller?.abort(); detail.clear(); context = null; entries = []; cursors = [null]; next = null; busy = false;
    body.replaceChildren(); count.textContent = '0'; status.textContent = ''; notice.hidden = true; more.hidden = true; empty.hidden = true;
    if (resetFilters) {
      const url = new URL(location.href);
      for (const key of filterKeys) url.searchParams.delete(queryKey(key));
      url.searchParams.delete('revision'); url.searchParams.delete('merge'); history.replaceState(null,'',url.pathname+url.search);
    }
  }
  function render() {
    const existing = new Map([...body.children].map(row => [row.dataset.merge,row]));
    for (const [index,entry] of entries.entries()) {
      let row = existing.get(entry.id);
      if (!row) { row = element('tr'); row.dataset.merge = entry.id; }
      if (body.children[index] !== row) body.insertBefore(row,body.children[index] || null);
      const signature = JSON.stringify(entry);
      if (row.dataset.version === signature) continue;
      row.dataset.version = signature; row.replaceChildren();
      const cells = headers.children;
      function cell(index) { const td = element('td'); td.dataset.label = cells[index].textContent; row.append(td); return td; }
      const direction = element('button','merge-open'); direction.type = 'button'; direction.id = `merge-${entry.id}`;
      direction.setAttribute('aria-label',`查看合并：${branchName(entry,'source')} → ${branchName(entry,'target')}`);
      direction.append(element('strong','',`${branchName(entry,'source')} → ${branchName(entry,'target')}`));
      const revisions = element('span','merge-inline-revisions'); revisions.append(revisionLabel(element,entry.source_revision), element('span','','→'), revisionLabel(element,entry.target_revision)); direction.append(revisions);
      direction.onclick = () => open(entry.id, direction.id); cell(0).append(direction);
      const state = cell(1); state.append(stateBadge(element,entry.status));
      if (entry.conflict_count) state.append(element('small','',`${entry.conflict_count} 项文件冲突`));
      if (!entry.history_complete) state.append(element('small','merge-history-incomplete','历史步骤未完整记录'));
      if (entry.error) state.append(element('small','merge-error-code',entry.error.code));
      cell(2).append(element('span',entry.remote_confirmed ? 'merge-confirmed' : 'merge-muted',syncLabel(entry.remote_confirmed)));
      const actor = cell(3); actor.append(element('span','',entry.principal || '身份未记录'));
      actor.append(element('small','',entry.origin_location === context.snapshot.status.location ? `本机 · ${short(entry.origin_location)}` : entry.origin_location ? `其他节点 · ${short(entry.origin_location)}` : '执行节点未记录'));
      const time = element('time','',timestamp(entry.created_ns)); if (entry.created_ns) time.dateTime = new Date(entry.created_ns / 1e6).toISOString(); cell(4).append(time);
      const outcome = mergeResult(entry), result = cell(5); result.append(revisionLabel(element,outcome.revision));
      if (outcome.unchanged) result.append(element('small','','现有目标版本 · 无需新版本'));
      else if (!entry.applied_revision && entry.candidate_revision) result.append(element('small','','合并候选'));
    }
    const known = new Set(entries.map(entry => entry.id)); for (const [id,row] of existing) if (!known.has(id)) row.remove();
    count.textContent = total; table.hidden = !entries.length; empty.hidden = !!entries.length || busy;
    more.hidden = !next; more.disabled = busy; renderFilters();
  }
  async function page(cursor, signal) {
    const params = new URLSearchParams({...filters,limit:'50'}); if (cursor) params.set('cursor',cursor);
    return api(`${endpoint(context.snapshot.status.workspace.id)}?${params}`,{signal});
  }
  async function load(mode = 'refresh') {
    if (!context || busy) return;
    const request = ++generation, loadedCursors = [...cursors];
    controller?.abort(); controller = new AbortController(); busy = true; more.disabled = true; notice.disabled = true;
    status.textContent = entries.length ? '正在更新合并记录…' : '正在读取合并记录…';
    try {
      const pages = [];
      if (mode === 'append') pages.push(await page(next, controller.signal));
      else if (mode === 'refresh' && entries.length) {
        for (const cursor of loadedCursors) pages.push(await page(cursor,controller.signal));
      } else {
        let cursor = null;
        for (let index = 0; index < (mode === 'replace' ? loadedCursors.length : 1); index++) {
          const result = await page(cursor,controller.signal); pages.push(result); cursor = result.next;
          if (!cursor) break;
        }
      }
      if (request !== generation) return;
      const received = pages.flatMap(value => value.entries), byId = new Map(received.map(value => [value.id,value]));
      total = pages[0].total;
      if (mode === 'refresh' && entries.length) {
        const previous = new Set(entries.map(entry => entry.id));
        notice.hidden = !received.some(entry => !previous.has(entry.id)) && entries.every(entry => byId.has(entry.id));
        entries = entries.map(entry => byId.get(entry.id) || entry);
        if (notice.hidden) next = pages.at(-1).next;
      } else if (mode === 'append') {
        cursors.push(next); const known = new Set(entries.map(entry => entry.id)); entries.push(...received.filter(entry => !known.has(entry.id))); next = pages[0].next;
      } else {
        entries = [...byId.values()]; cursors = [null,...pages.slice(0,-1).map(value => value.next)]; next = pages.at(-1).next; notice.hidden = true;
      }
      notice.textContent = '记录已更新，查看更新';
      status.textContent = `已显示 ${entries.length} / ${total} 条记录 · 读取时间 ${timestamp(pages[0].observed_ns)}`;
      const active = new URLSearchParams(location.search).get('merge');
      if (active) await detail.show(context.snapshot.status.workspace.id, active, {refresh:true});
    } catch (error) {
      if (request !== generation || error.name === 'AbortError') return;
      status.textContent = `合并记录读取失败：${error.message}${entries.length ? '。当前列表已保留。' : ''}`;
      notice.hidden = false; notice.textContent = '重新加载合并记录';
    } finally {
      if (request === generation) { busy = false; notice.disabled = false; render(); }
    }
  }
  async function open(id, triggerId) {
    const url = new URL(location.href); url.searchParams.set('merge',id); history.replaceState(null,'',url.pathname+url.search);
    await detail.show(context.snapshot.status.workspace.id, id, {triggerId});
  }
  function applyFilters() {
    const url = new URL(location.href);
    for (const key of filterKeys) {
      const raw = inputs[key].value;
      const value = key.endsWith('_ns') && raw ? String(new Date(raw).getTime() * 1e6) : raw;
      if (value) { filters[key] = value; url.searchParams.set(queryKey(key),value); }
      else { delete filters[key]; url.searchParams.delete(queryKey(key)); }
    }
    if (filters.revision) url.searchParams.set('revision',filters.revision); else url.searchParams.delete('revision');
    url.searchParams.delete('merge'); history.replaceState(null,'',url.pathname+url.search);
    generation++; controller?.abort(); busy = false; detail.clear(); entries = []; cursors = [null]; next = null; body.replaceChildren();
    renderFilters(); load('initial');
  }
  async function show(snapshot, selectedBranch) {
    if (!snapshot) { clear(); return; }
    const changed = context?.snapshot.status.workspace.id !== snapshot.status.workspace.id;
    if (changed) clear(); context = {snapshot,selectedBranch};
    if (changed) readFilters(); renderFilters(); await load(changed ? 'initial' : 'refresh');
  }
  form.onsubmit = event => { event.preventDefault(); applyFilters(); };
  reset.onclick = () => { for (const input of Object.values(inputs)) input.value = ''; delete filters.revision; applyFilters(); };
  selected.onclick = () => { inputs.branch.value = context.selectedBranch; applyFilters(); };
  more.onclick = () => load('append'); notice.onclick = () => load('replace');
  return {show,clear};
}
