export const mergeStatuses = {
  conflicts:'文件冲突待处理', pending:'尚未应用', waiting_for_mount:'等待挂载处理',
  applied:'已应用', failed:'操作失败', recovery_required:'需要恢复', already_included:'来源版本已包含',
};
export const short = value => value ? value.slice(0, 8) : '未记录';
export const timestamp = value => value ? new Date(value / 1e6).toLocaleString('zh-CN', {hour12:false}) : '未记录';
export const branchName = (entry, side) => entry[`${side}_name`] || (entry[`${side}_branch`] ? short(entry[`${side}_branch`]) : '分支未记录');
export function mergeResult(entry) {
  if (entry.status === 'already_included') return {revision:entry.target_revision,label:'现有目标版本',unchanged:true};
  return {revision:entry.applied_revision || entry.candidate_revision,label:entry.applied_revision ? '合并结果' : '当前候选',unchanged:false};
}
export function syncLabel(value) {
  return value === true ? '远端已确认' : value === false ? '远端待确认' : '远端状态未记录';
}
export function stateBadge(element, status) {
  return element('span', `pill merge-state ${['failed','recovery_required'].includes(status) ? 'merge-state-error' : ['applied','already_included'].includes(status) ? '' : 'paused'}`, mergeStatuses[status] || '状态未记录');
}
export function revisionLabel(element, revision, prefix = '') {
  const code = element('code', 'merge-revision', `${prefix}${short(revision)}`);
  if (revision) code.title = revision;
  return code;
}
