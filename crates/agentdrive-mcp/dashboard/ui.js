const paths = {
  archive:['M4 4h16v4H4z','M5 8v12h14V8','M9 12h6'],
  branch:['M6 3v12','M18 6v3a6 6 0 0 1-6 6H6','M6 15a3 3 0 1 0 0 6 3 3 0 0 0 0-6','M18 3a3 3 0 1 0 0 6 3 3 0 0 0 0-6'],
  history:['M3 11a9 9 0 1 1 2.6 7.4','M3 4v7h7','M12 7v5l3 2'],
  drive:['M5 4h14l3 12v4H2v-4z','M2 16h20','M6 18h.01M10 18h.01'],
  box:['M12 3 3 8v9l9 5 9-5V8z','M3 8l9 5 9-5','M12 13v9','M7.5 5.5l9 5'],
  settings:['M9 3h6l1 3 3 1 2 5-2 5-3 1-1 3H9l-1-3-3-1-2-5 2-5 3-1z','M15 12a3 3 0 1 0-6 0 3 3 0 0 0 6 0'],
  refresh:['M20 7a9 9 0 0 0-15-2L3 7','M3 3v4h4','M4 17a9 9 0 0 0 15 2l2-2','M17 17h4v4'],
  chevronLeft:['m14 6-6 6 6 6'],
  chevronRight:['m10 6 6 6-6 6'],
  close:['m6 6 12 12','M18 6 6 18'],
  trash:['M3 6h18','M9 6V3h6v3','M5 6l1 15h12l1-15','M10 10v7M14 10v7'],
  folder:['M3 5h6l2 2h10v13H3z'],
  file:['M5 3h9l5 5v13H5z','M14 3v6h5','M9 13h6M9 17h6'],
  external:['M14 3h7v7','m21 3-9 9','M10 3H3v18h18v-7'],
};

export function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  for (const [key,value] of Object.entries({viewBox:'0 0 24 24',fill:'none',stroke:'currentColor','stroke-width':'1.75','stroke-linecap':'round','stroke-linejoin':'round','aria-hidden':'true',focusable:'false',class:'ui-icon'})) svg.setAttribute(key,value);
  for (const d of paths[name] || paths.file) {
    const path = document.createElementNS(svg.namespaceURI, 'path'); path.setAttribute('d',d); svg.append(path);
  }
  return svg;
}

export function decorateIcon(node, className, text) {
  const names = {'branch-symbol':'branch','archive-branch-symbol':'branch','sandbox-template-icon':'box','revision-dialog-close':'close','workspace-delete-button':'trash'};
  const name = className?.split(' ').map(value => names[value]).find(Boolean);
  if (name) node.replaceChildren(icon(name));
  else if (className?.split(' ').includes('file-symbol')) node.replaceChildren(icon(text === '▰' || text === '▸' ? 'folder' : 'file'));
}

export function initializeIcons() {
  for (const node of document.querySelectorAll('[data-icon]')) node.replaceChildren(icon(node.dataset.icon));
}
