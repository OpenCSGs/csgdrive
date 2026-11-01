import {createRevisionChanges} from './changes.js';
import {short, timestamp, branchName, mergeResult, syncLabel, stateBadge, revisionLabel} from './merge-presentation.js';

const actions = {merge_prepare:'准备合并',merge_resolve:'更新候选',merge_apply:'应用合并',mount_release:'恢复挂载'};
const phases = {reserved:'请求已记录',waiting_for_quiesce:'等待释放挂载',saving:'正在保存',local_saved:'本机已保存',uploading:'正在上传',committed_mount_pending:'已保存，等待挂载恢复',complete:'操作完成',cancelled:'已取消',recovery_required:'需要恢复',failed:'操作失败'};
const choices = {source:'采用来源内容',target:'保留目标内容',delete:'删除',remove:'删除',copy:'复制',replace:'替换内容'};
const endpoint = (workspace,id) => `/api/workspaces/${encodeURIComponent(workspace)}/merges/${encodeURIComponent(id)}`;

export function createMergeDetail({api, element, closed, viewRevision}) {
  const dialog = element('dialog','revision-dialog merge-dialog'); dialog.id = 'merge-dialog'; dialog.setAttribute('aria-labelledby','merge-dialog-title');
  const heading = element('div','revision-dialog-heading'), copy = element('div');
  const title = element('h2','','合并详情'); title.id = 'merge-dialog-title';
  const subtitle = element('p','revision-dialog-subtitle'); copy.append(title,subtitle);
  const close = element('button','revision-dialog-close','×'); close.type = 'button'; close.autofocus = true; close.setAttribute('aria-label','关闭合并详情'); heading.append(copy,close);
  const feedback = element('p','merge-detail-feedback'); feedback.setAttribute('role','status');
  const retry = element('button','secondary','重新读取合并详情'); retry.type = 'button'; retry.hidden = true;
  const overview = element('section','merge-overview'), states = element('div','merge-detail-states'), observed = element('p','merge-muted');
  const incomplete = element('p','merge-history-warning','历史步骤未完整记录；当前展示能够读取的操作和版本。'); incomplete.hidden = true;
  const relation = element('div','merge-version-relation'); relation.setAttribute('aria-label','合并版本关系');
  const metadata = element('details','merge-metadata'), metadataTitle = element('summary','','记录标识'), metadataGrid = element('dl','detail-grid'); metadata.append(metadataTitle,metadataGrid);
  overview.append(states,observed,incomplete,relation,metadata);
  const timelineSection = element('section','merge-timeline-section'), timelineHeading = element('h3','revision-section-title','动作时间线'), eventCount = element('span','count-badge','0'); timelineHeading.append(eventCount);
  const timeline = element('ol','merge-timeline'); timeline.id = 'merge-events';
  const eventStatus = element('p','merge-muted'); eventStatus.setAttribute('role','status');
  const more = element('button','secondary','加载更多合并动作'); more.type = 'button'; more.hidden = true;
  timelineSection.append(timelineHeading,timeline,eventStatus,more);
  const changesSection = element('section','merge-changes-section'), changesHeading = element('h3','revision-section-title','文件变化');
  const comparisonLabel = element('label','merge-comparison-label','比较版本'), select = element('select'); select.id = 'merge-comparison'; select.setAttribute('aria-label','比较版本'); comparisonLabel.append(select);
  const changes = element('div','revision-changes'); changesSection.append(changesHeading,comparisonLabel,changes);
  dialog.append(heading,feedback,retry,overview,timelineSection,changesSection); document.body.append(dialog);
  const diff = createRevisionChanges({api,element,container:changes});
  let context = null, generation = 0, controller = null, eventController = null, events = [], eventNext = null, eventTotal = 0, eventBusy = false, data = null, comparisons = new Map(), revisions = new Map();
  function clear(notify = false) {
    const trigger = context?.triggerId;
    generation++; controller?.abort(); eventController?.abort(); context = null; data = null; events = []; eventNext = null; eventBusy = false; comparisons.clear(); revisions.clear(); diff.clear();
    timeline.replaceChildren(); overview.hidden = true; timelineSection.hidden = true; changesSection.hidden = true;
    feedback.textContent = ''; retry.hidden = true; select.replaceChildren();
    if (dialog.open) dialog.close();
    if (notify) { closed?.(); document.getElementById(trigger)?.focus({preventScroll:true}); }
  }
  function addRevisions(values) { for (const revision of values || []) revisions.set(revision.id,revision); }
  function versionCard(label,id,branch) {
    const card = element('div','merge-version-card'); card.append(element('span','merge-muted',label));
    if (branch) card.append(element('strong','',branch));
    if (id && revisions.has(id) && viewRevision) {
      const link = element('button','merge-version-link',short(id)); link.type = 'button'; link.title = id; link.id = `merge-version-${label}-${id}`;
      link.onclick = () => viewRevision({workspace:context.workspace,revision:revisions.get(id),message:label,sourceBranch:branch || '未记录',isHead:false,triggerId:link.id});
      card.append(link);
    } else card.append(revisionLabel(element,id));
    return card;
  }
  function renderOverview() {
    const entry = data.entry, outcome = mergeResult(entry);
    title.textContent = `${branchName(entry,'source')} → ${branchName(entry,'target')}`;
    subtitle.textContent = `发起于 ${timestamp(entry.created_ns)} · ${entry.principal || '身份未记录'}`;
    states.replaceChildren(stateBadge(element,entry.status),element('span',entry.remote_confirmed === true ? 'pill' : 'pill paused',syncLabel(entry.remote_confirmed)));
    if (entry.mount_ready !== null && entry.mount_ready !== undefined) states.append(element('span',entry.mount_ready ? 'pill' : 'pill paused',entry.mount_ready ? '记录时挂载已就绪' : '记录时挂载未就绪'));
    if (entry.conflict_count) states.append(element('span','pill paused',`${entry.conflict_count} 项文件冲突`));
    observed.textContent = `${entry.origin_location === data.location ? '本机记录' : entry.origin_location ? `执行节点 ${short(entry.origin_location)}` : '执行节点未记录'} · 读取时间 ${timestamp(data.observed_ns)}`;
    incomplete.hidden = entry.history_complete;
    relation.replaceChildren();
    const parents = element('div','merge-parent-versions'); parents.append(versionCard('目标合并前',entry.target_revision,branchName(entry,'target')),versionCard('来源版本',entry.source_revision,branchName(entry,'source')));
    const arrow = element('span','merge-relation-arrow','→'); arrow.setAttribute('aria-hidden','true');
    const resultCard = versionCard(outcome.label,outcome.revision,branchName(entry,'target'));
    if (outcome.unchanged) resultCard.append(element('small','merge-muted','来源版本已包含，无需生成新版本。'));
    relation.append(parents,arrow,resultCard);
    metadataGrid.replaceChildren();
    for (const [label,value] of [['合并 ID',entry.merge_id],['记录 ID',entry.id],['共同版本',entry.base_revision],['来源版本',entry.source_revision],['目标原版本',entry.target_revision],['候选版本',outcome.unchanged ? '无需生成' : entry.candidate_revision],[outcome.unchanged ? '新结果版本' : '结果版本',outcome.unchanged ? '无需生成' : entry.applied_revision],['执行节点',entry.origin_location]]) {
      const field = element('div','detail-field'); field.append(element('dt','',label),element('dd','',value || '未记录')); metadataGrid.append(field);
    }
    overview.hidden = false;
  }
  function selectComparison(key) {
    if (comparisons.has(key)) select.value = key;
    const pair = comparisons.get(select.value);
    if (!pair) { diff.clear(); return; }
    diff.show(context.workspace,{id:pair.after,parents:[]},{before:pair.before,summary:`${pair.label} · ${short(pair.before)} → ${short(pair.after)}`});
  }
  function renderComparisons() {
    const selected = select.value, entry = data.entry;
    comparisons = new Map();
    function pair(label,before,after) {
      if (!before || !after || before === after) return;
      const key = `${before}/${after}`; if (!comparisons.has(key)) comparisons.set(key,{label,before,after});
    }
    const outcome = mergeResult(entry), after = outcome.revision;
    pair(`目标合并前 → ${outcome.label}`,entry.target_revision,after);
    pair(`来源版本 → ${outcome.label}`,entry.source_revision,after);
    pair(`共同版本 → ${outcome.label}`,entry.base_revision,after);
    for (const event of events) if (event.candidate_before && event.candidate_revision) pair(`候选更新 · ${short(event.candidate_revision)}`,event.candidate_before,event.candidate_revision);
    // A reader keeps the fixed comparison selected while newer candidates arrive.
    const old = select.selectedOptions[0];
    if (selected && !comparisons.has(selected) && old) {
      const [before,after] = selected.split('/'); comparisons.set(selected,{before,after,label:'先前选择'});
    }
    select.replaceChildren(...[...comparisons].map(([value,pair]) => new Option(`${pair.label} · ${short(pair.before)} → ${short(pair.after)}`,value)));
    select.value = comparisons.has(selected) ? selected : comparisons.keys().next().value || '';
    changesSection.hidden = !comparisons.size; selectComparison();
  }
  function renderEvents() {
    const old = new Map([...timeline.children].map(row => [row.dataset.event,row]));
    for (const [index,event] of events.entries()) {
      let row = old.get(event.id); if (!row) { row = element('li','merge-event'); row.dataset.event = event.id; }
      if (timeline.children[index] !== row) timeline.insertBefore(row,timeline.children[index] || null);
      const stamp = JSON.stringify(event); if (row.dataset.version === stamp) continue;
      row.dataset.version = stamp; row.replaceChildren();
      const header = element('div','merge-event-heading'); header.append(element('strong','',actions[event.name] || event.name),element('span',event.error ? 'merge-error-code' : 'merge-muted',phases[event.phase] || event.phase),element('time','',timestamp(event.created_ns))); row.append(header);
      const copy = element('div','merge-event-copy');
      if (event.candidate_revision) copy.append(revisionLabel(element,event.candidate_revision,'候选 '));
      if (event.applied_revision) copy.append(revisionLabel(element,event.applied_revision,data.entry.status === 'already_included' ? '现有目标 ' : '结果 '));
      if (event.conflict_count !== null && event.conflict_count !== undefined) copy.append(element('span','',`文件冲突 ${event.conflict_count} 项`));
      if (event.remote_confirmed) copy.append(element('span','merge-confirmed','远端已确认'));
      if (event.mount_ready === true) copy.append(element('span','','挂载已就绪'));
      if (event.error) copy.append(element('p','merge-event-error',`${event.error.code}：${event.error.message}`));
      if (!event.history_complete) copy.append(element('span','merge-history-incomplete','中间状态未记录'));
      if (event.paths?.length) {
        const paths = element('ul','merge-event-paths');
        for (const entry of event.paths) { const item = element('li'); item.append(element('code','',entry.path),element('span','',choices[entry.choice] || entry.choice)); paths.append(item); }
        copy.append(paths);
      }
      if (event.candidate_before && event.candidate_revision && event.candidate_before !== event.candidate_revision) {
        const compare = element('button','merge-text-button','查看本次候选变化'); compare.type = 'button';
        compare.onclick = () => { selectComparison(`${event.candidate_before}/${event.candidate_revision}`); changesSection.scrollIntoView({block:'start'}); };
        copy.append(compare);
      }
      const details = element('details','merge-event-identifiers'); details.append(element('summary','','操作详情'));
      const values = element('p','merge-muted'); values.append(document.createTextNode(`调用身份：${event.principal || '未记录'} · 节点：${event.location || '未记录'}`));
      const identifiers = element('pre','',`operation: ${event.operation}\nrequest_id: ${event.request_id}`); details.append(values,identifiers); copy.append(details); row.append(copy);
    }
    eventCount.textContent = eventTotal; eventStatus.textContent = events.length ? `已显示 ${events.length} / ${eventTotal} 条动作` : '没有可读取的动作记录。';
    more.hidden = !eventNext; more.disabled = eventBusy; timelineSection.hidden = false;
  }
  async function loadEvents() {
    if (!context || !eventNext || eventBusy) return;
    const active = context, request = generation;
    eventController?.abort(); eventController = new AbortController(); eventBusy = true; more.disabled = true;
    try {
      const result = await api(`${endpoint(active.workspace,active.id)}/events?${new URLSearchParams({cursor:eventNext,limit:'50'})}`,{signal:eventController.signal});
      if (request !== generation) return;
      const known = new Set(events.map(event => event.id)); events.push(...result.entries.filter(event => !known.has(event.id))); addRevisions(result.revisions);
      eventNext = result.next; eventTotal = result.total; renderEvents(); renderComparisons();
    } catch (error) { if (request === generation && error.name !== 'AbortError') eventStatus.textContent = `动作读取失败：${error.message}`; }
    finally { if (request === generation) { eventBusy = false; more.disabled = false; } }
  }
  async function show(workspace,id,{refresh = false,triggerId} = {}) {
    const same = context?.workspace === workspace && context.id === id;
    if (!same) { clear(); context = {workspace,id,triggerId}; title.textContent = '合并详情'; subtitle.textContent = ''; feedback.textContent = '正在读取合并记录…'; dialog.showModal(); dialog.scrollTop = 0; }
    else if (triggerId) context.triggerId = triggerId;
    if (same && refresh && (controller || eventBusy)) return;
    const request = ++generation; controller?.abort(); controller = new AbortController(); retry.hidden = true;
    try {
      const result = await api(endpoint(workspace,id),{signal:controller.signal});
      if (request !== generation) return;
      const loaded = [...result.events.entries]; let cursor = result.events.next;
      while (same && loaded.length < events.length && cursor) {
        const page = await api(`${endpoint(workspace,id)}/events?${new URLSearchParams({cursor,limit:'50'})}`,{signal:controller.signal});
        if (request !== generation) return;
        loaded.push(...page.entries); result.events.revisions.push(...page.revisions); cursor = page.next;
      }
      data = result; addRevisions(result.revisions); addRevisions(result.events.revisions);
      const known = new Map(events.map(event => [event.id,event])); for (const event of loaded) known.set(event.id,event);
      events = [...known.values()].sort((a,b) => a.created_ns - b.created_ns || a.id.localeCompare(b.id));
      eventNext = cursor;
      eventTotal = result.events.total;
      feedback.textContent = result.entry.error ? `${result.entry.error.code}：${result.entry.error.message}` : '';
      renderOverview(); renderEvents(); renderComparisons();
    } catch (error) {
      if (request !== generation || error.name === 'AbortError') return;
      feedback.textContent = `合并详情读取失败：${error.message}`; retry.hidden = false;
    } finally { if (request === generation) controller = null; }
  }
  close.onclick = () => clear(true); dialog.addEventListener('cancel',event => {event.preventDefault();clear(true);});
  retry.onclick = () => { if (context) show(context.workspace,context.id); };
  more.onclick = loadEvents; select.onchange = () => selectComparison();
  return {show,clear};
}
