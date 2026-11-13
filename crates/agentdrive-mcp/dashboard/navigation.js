import { icon } from './ui.js';

export function createNavigation({element, $, navigate, deleteWorkspace}) {
  const pages = [
    ['archive-nav', '/archive', '档案室'],
    ['history-nav', '/dashboard', '分支与版本'],
    ['mounts-nav', '/mounts', '挂载点管理'],
    ['branches-nav', '/branches', '分支管理'],
    ['sandboxes-nav', '/sandboxes', '沙盒'],
  ];
  let workspaces = [], selected = null, listKey = '';
  let activePath = '';
  const compact = window.matchMedia('(max-width: 900px)');
  let collapsed = localStorage.getItem('agentdrive-workspaces-collapsed');
  collapsed = collapsed === null ? compact.matches : collapsed === 'true';

  function setCollapsed(value, persist = true) {
    collapsed = value;
    $('app').classList.toggle('workspaces-collapsed', value);
    const label = value ? '展开工作区列表' : '收起工作区列表';
    $('workspace-toggle').setAttribute('aria-expanded', String(!value));
    $('workspace-toggle').setAttribute('aria-label', label);
    $('workspace-toggle').title = label;
    $('workspace-toggle').replaceChildren(icon(value ? 'chevronRight' : 'chevronLeft'));
    $('workspace-backdrop').hidden = value;
    if (persist) localStorage.setItem('agentdrive-workspaces-collapsed', String(value));
    if (value) { $('workspace-search').value = ''; filter(); }
  }
  function revealCurrentPage() {
    if (compact.matches) requestAnimationFrame(() => $('workspace-menu').querySelector('[aria-current="page"]')?.scrollIntoView({block:'nearest', inline:'nearest'}));
  }
  compact.addEventListener('change', event => { if (event.matches) { setCollapsed(true, false); revealCurrentPage(); } });
  $('workspace-toggle').onclick = () => setCollapsed(!collapsed);
  $('workspace-backdrop').onclick = () => setCollapsed(true);
  $('workspace-sidebar').addEventListener('keydown', event => {
    if (event.key === 'Escape' && !collapsed) { setCollapsed(true); $('workspace-toggle').focus(); }
  });
  function filter() {
    const query = $('workspace-search').value.trim().toLocaleLowerCase();
    let count = 0;
    for (const link of $('workspace-list').children) {
      link.hidden = !link.dataset.name.includes(query);
      if (!link.hidden) count++;
    }
    $('workspace-list-empty').hidden = count > 0;
    $('workspace-list-empty').textContent = workspaces.length ? '没有匹配的工作区' : '暂无工作区';
  }
  $('workspace-search').addEventListener('input', filter);
  function follow(event, url) {
    if (event.button || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return false;
    event.preventDefault();
    if (compact.matches) setCollapsed(true);
    history.pushState(null, '', url.pathname + url.search);
    navigate();
    return true;
  }
  $('workspace-list').addEventListener('click', event => {
    const link = event.target.closest('a[data-workspace]');
    if (link) follow(event, new URL(link.href));
  });
  for (const [id, path] of pages) $(id).addEventListener('click', event => {
    if (!selected && path !== '/sandboxes') { event.preventDefault(); return; }
    const url = new URL(path, location.origin);
    if (selected) url.searchParams.set('workspace', selected.id);
    const branch = new URLSearchParams(location.search).get('branch');
    if (branch) url.searchParams.set('branch', branch);
    follow(event, url);
  });
  $('settings-nav').addEventListener('click', event => follow(event, new URL('/settings', location.origin)));
  $('brand-link').addEventListener('click', event => follow(event, new URL('/archive', location.origin)));

  function page() {
    const settings = location.pathname === '/settings';
    $('app').classList.toggle('settings-view', settings);
    $('workspace-menu').hidden = settings;
    const active = pages.find(([,path]) => path === location.pathname) || pages[0];
    for (const [id, path] of [...pages, ['settings-nav', '/settings', '设置']]) {
      const current = path === location.pathname || (location.pathname === '/' && path === '/archive');
      $(id).classList.toggle('nav-active', current);
      if (current) $(id).setAttribute('aria-current', 'page'); else $(id).removeAttribute('aria-current');
    }
    $('page-name').textContent = active[2];
    if (activePath !== location.pathname) { activePath = location.pathname; revealCurrentPage(); }
    document.title = `AgentDrive · ${settings ? '设置' : active[2]}`;
  }
  function update(items) {
    workspaces = items;
    const url = new URL(location.href), settings = url.pathname === '/settings';
    const requested = url.searchParams.get('workspace');
    selected = items.find(value => value.id === (requested || (settings ? selected?.id : null))) || items[0] || null;
    if (!settings && requested !== selected?.id) {
      url.search = '';
      if (selected) url.searchParams.set('workspace', selected.id);
      // Preserve direct links without an explicit workspace on initial entry.
      if (!requested) for (const [key,value] of new URLSearchParams(location.search)) url.searchParams.set(key,value);
      history.replaceState(null, '', url.pathname + url.search);
    } else if (settings && url.search) history.replaceState(null, '', '/settings');
    const key = JSON.stringify(items.map(value => [value.id,value.name]));
    if (key !== listKey) {
      const focused = document.activeElement?.dataset.workspace, scroll = $('workspace-list').scrollTop;
      $('workspace-list').replaceChildren(...items.map(workspace => {
        const row = element('div', 'workspace-row'); row.dataset.name = workspace.name.toLocaleLowerCase();
        const link = element('a', 'workspace-link');
        const url = new URL('/archive', location.origin); url.searchParams.set('workspace', workspace.id);
        link.href = url.pathname + url.search; link.dataset.workspace = workspace.id;
        link.dataset.name = workspace.name.toLocaleLowerCase(); link.title = workspace.name;
        link.setAttribute('aria-label', workspace.name);
        const icon = element('span', 'workspace-avatar', Array.from(workspace.name)[0]?.toLocaleUpperCase() || 'W');
        icon.setAttribute('aria-hidden', 'true');
        link.append(icon, element('span', 'workspace-title', workspace.name));
        const remove = element('button', 'workspace-delete-button', '删除'); remove.type = 'button';
        remove.setAttribute('aria-label', `删除工作区 ${workspace.name}`); remove.setAttribute('aria-haspopup', 'dialog');
        remove.onclick = () => deleteWorkspace(workspace, remove);
        row.append(link, remove); return row;
      }));
      $('workspace-list').scrollTop = scroll;
      if (focused) [...$('workspace-list').querySelectorAll('a[data-workspace]')].find(link => link.dataset.workspace === focused)?.focus({preventScroll:true});
      listKey = key;
    }
    for (const link of $('workspace-list').querySelectorAll('a[data-workspace]')) {
      const active = !settings && link.dataset.workspace === selected?.id;
      link.classList.toggle('workspace-active', active);
      if (active) link.setAttribute('aria-current', 'true'); else link.removeAttribute('aria-current');
    }
    $('workspace-total').textContent = items.length;
    $('workspace-menu-name').textContent = selected?.name || '暂无工作区';
    $('workspace-menu-name').title = selected?.name || '';
    $('workspace-name').textContent = selected?.name || '暂无工作区';
    for (const [id,path] of pages) {
      $(id).setAttribute('aria-disabled', String(!selected && path !== '/sandboxes'));
      $(id).tabIndex = selected || path === '/sandboxes' ? 0 : -1;
      const url = new URL(path, location.origin);
      if (selected) url.searchParams.set('workspace', selected.id);
      $(id).href = url.pathname + url.search;
    }
    filter(); page();
    return selected?.id;
  }
  setCollapsed(collapsed, false);
  return {update, page};
}
