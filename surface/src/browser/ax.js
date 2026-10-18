// Renders a page's accessibility tree (CDP Accessibility.getFullAXTree) in
// the line format of native observations:
//   [id] role "name" (states) desc="…" value=N text="…" @x,y wxh
// IDs are the nodes' backendDOMNodeId, stable for the life of the document.
// Pruning: ignored nodes and unnamed generic wrappers are dissolved (their
// children move up), inline text boxes and list markers are dropped, text
// that only repeats a name is dropped, a container whose content is only
// text shows it as text="…", and unnamed decorative leaves disappear.
// Coordinates are CSS pixels of the viewport (what click([x, y]) and the
// screenshot use), shown except on named widgets; elements scrolled out of
// view are marked (offscreen).

const MAX_NAME = 200;
const MAX_TEXT = 300;
export const MAX_OBSERVATION_CHARS = 16_000;

const DROPPED = new Set(['InlineTextBox', 'LineBreak', 'ListMarker']);
// Wrappers without meaning of their own when unnamed.
const WRAPPERS = new Set(['generic', 'none', 'presentation', 'group', 'Section', 'section', 'GenericContainer', 'Div',
  'LayoutTable', 'LayoutTableRow', 'LayoutTableCell', 'Pre', 'strong', 'emphasis', 'code', 'mark', 'time', 'subscript',
  'superscript', 'deletion', 'insertion', 'Abbr', 'Ruby', 'Legend', 'Unknown', 'MenuListPopup']);
// Fields whose content is their text="…"; the inner editor nodes repeat it.
const FIELDS = new Set(['textbox', 'searchbox']);
const INTERACTIVE = new Set(['button', 'link', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio', 'switch', 'slider',
  'spinbutton', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'tab', 'option', 'treeitem', 'listbox', 'PopUpButton',
  'ToggleButton', 'DisclosureTriangle', 'scrollbar']);
const NUMERIC = new Set(['slider', 'progressbar', 'spinbutton', 'scrollbar', 'meter']);
const TOGGLES = new Set(['checkbox', 'radio', 'switch', 'menuitemcheckbox', 'menuitemradio']);
// As compactTree does for native trees: a named widget is acted on by its ID,
// so its line carries no coordinates (its element keeps them).
const WIDGETS = new Set(['button', 'link', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio', 'switch', 'slider',
  'spinbutton', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'tab', 'option', 'treeitem', 'heading', 'listitem',
  'cell', 'gridcell', 'columnheader', 'rowheader', 'label', 'static text']);
const ROLE_NAMES = { StaticText: 'static text', RootWebArea: 'document', WebArea: 'document', LabelText: 'label', img: 'image', PopUpButton: 'combobox', ToggleButton: 'button' };

// CDP role spellings to the words models use ("DescriptionList" -> "description list").
export function roleName(role) {
  if (ROLE_NAMES[role]) return ROLE_NAMES[role];
  return /[A-Z]/.test(role) ? role.replace(/([a-z])([A-Z])/g, '$1 $2').toLowerCase() : role;
}

const clip = (s, max) => (s.length > max ? `${s.slice(0, max)}…` : s);
const clean = (s) => String(s ?? '').replace(/\s+/g, ' ').trim();
const quote = (s, max) => JSON.stringify(clip(s, max));
// Text that only repeats the name (up to spacing) says nothing new.
const same = (a, b) => a.replace(/\s+/g, '') === b.replace(/\s+/g, '');
const val = (o) => (o && o.value !== undefined ? o.value : undefined);

function properties(n) {
  const p = {};
  for (const x of n.properties || []) p[x.name] = val(x.value);
  return p;
}

function states(role, p) {
  const s = [];
  if (p.focused) s.push('focused');
  if (p.disabled) s.push('disabled');
  if (p.selected === true) s.push('selected');
  if (p.checked === 'mixed') s.push('mixed');
  else if (p.checked === true || p.checked === 'true') s.push('checked');
  else if (TOGGLES.has(role)) s.push('unchecked');
  if (p.pressed === true || p.pressed === 'true') s.push('pressed');
  if (p.expanded === true) s.push('expanded');
  else if (p.expanded === false) s.push('collapsed');
  if (p.readonly) s.push('readonly');
  else if (p.editable && (role === 'textbox' || role === 'searchbox' || role === 'combobox')) s.push('editable');
  if (p.modal) s.push('modal');
  if (p.required) s.push('required');
  if (p.invalid && p.invalid !== 'false') s.push('invalid');
  if (p.busy) s.push('busy');
  return s;
}

// backendNodeId -> [x, y, w, h] in viewport CSS pixels, from
// DOMSnapshot.captureSnapshot({computedStyles: []}) of the main document.
export function rectsFromSnapshot(snap) {
  const rects = new Map();
  const doc = snap?.documents?.[0];
  if (!doc?.layout || !doc.nodes?.backendNodeId) return rects;
  const sx = doc.scrollOffsetX || 0;
  const sy = doc.scrollOffsetY || 0;
  const { nodeIndex, bounds } = doc.layout;
  for (let i = 0; i < nodeIndex.length; i++) {
    const id = doc.nodes.backendNodeId[nodeIndex[i]];
    const b = bounds[i];
    if (id === undefined || !b || rects.has(id)) continue;
    rects.set(id, [Math.round(b[0] - sx), Math.round(b[1] - sy), Math.round(b[2]), Math.round(b[3])]);
  }
  return rects;
}

// {lines, elements, total}: one line and one element per shown node, in
// document order. Elements are what locators match:
// {id, role, name, states, desc?, value?, text?, rect?, offscreen?, depth}.
export function renderAxTree(nodes, { rects = new Map(), viewport = null } = {}) {
  const byId = new Map();
  for (const n of nodes || []) byId.set(n.nodeId, n);
  const root = (nodes || []).find((n) => n.parentId === undefined) || (nodes || [])[0];
  const lines = [];
  const elements = [];
  if (!root) return { lines, elements, total: 0 };

  const rawRole = (n) => String(val(n.role) ?? '');
  const nameOf = (n) => clean(val(n.name));
  const seen = new Set();
  // Shown children of n: dissolved wrappers and ignored nodes are replaced by
  // their own children; static text stays as {text} items.
  const items = (n, out = []) => {
    for (const cid of n.childIds || []) {
      const c = byId.get(cid);
      if (!c || seen.has(c.nodeId)) continue;
      seen.add(c.nodeId);
      const role = rawRole(c);
      if (DROPPED.has(role)) continue;
      if (c.ignored || c.backendDOMNodeId === undefined) { items(c, out); continue; }
      if (role === 'StaticText') { const t = nameOf(c); if (t) out.push({ text: t, node: c }); continue; }
      const p = properties(c);
      if (WRAPPERS.has(role) && !nameOf(c) && !p.focusable && !p.editable) { items(c, out); continue; }
      out.push({ node: c });
    }
    return out;
  };

  const make = (n, depth, role, name, text) => {
    const p = properties(n);
    const el = { id: n.backendDOMNodeId, role: roleName(role), name, states: states(role, p), depth };
    const desc = clean(val(n.description));
    if (desc && desc !== name) el.desc = desc;
    const v = clean(val(n.value));
    if (v && NUMERIC.has(role) && !/\s/.test(v)) el.value = v;
    else if (v && !same(v, name)) el.text = v;
    else if (text) el.text = text;
    const r = rects.get(n.backendDOMNodeId);
    if (r) {
      el.rect = r;
      if (viewport && r[2] > 0 && r[3] > 0 && (r[0] + r[2] <= 0 || r[1] + r[3] <= 0 || r[0] >= viewport.width || r[1] >= viewport.height)) el.offscreen = true;
    }
    el.interactive = INTERACTIVE.has(role) || !!p.focusable;
    return el;
  };

  // Builds n's subtree into `out`; returns whether anything was emitted.
  const build = (n, depth, out) => {
    const role = rawRole(n);
    const name = nameOf(n);
    const kids = FIELDS.has(role) ? [] : items(n);
    const hasElements = kids.some((k) => k.text === undefined);
    let text;
    if (!hasElements && kids.length) {
      const joined = clean(kids.map((k) => k.text).join(' '));
      if (joined && !same(joined, name)) text = joined;
    }
    const el = make(n, depth, role, name, text);
    const sub = [];
    if (hasElements) {
      for (const k of kids) {
        if (k.text !== undefined) {
          if (same(k.text, name)) continue;
          sub.push(make(k.node, depth + 1, 'StaticText', k.text));
        } else {
          build(k.node, depth + 1, sub);
        }
      }
    }
    // An unnamed, stateless, non-interactive leaf says nothing.
    if (!sub.length && !el.name && !el.text && !el.value && !el.desc && !el.interactive && !el.states.length && n !== root) return;
    out.push(el, ...sub);
  };
  build(root, 0, elements);
  for (const el of elements) {
    lines.push(formatElement(el));
    delete el.interactive;
  }
  return { lines, elements, total: elements.length };
}

export function formatElement(el) {
  let s = `${'  '.repeat(el.depth)}[${el.id}] ${el.role}`;
  if (el.name) s += ` ${quote(el.name, MAX_NAME)}`;
  if (el.states.length) s += ` (${el.states.join(', ')})`;
  if (el.desc) s += ` desc=${quote(el.desc, 120)}`;
  if (el.value !== undefined) s += ` value=${el.value}`;
  if (el.text !== undefined) s += ` text=${quote(el.text, MAX_TEXT)}`;
  if (el.rect && !(el.name && WIDGETS.has(el.role))) s += ` @${el.rect[0]},${el.rect[1]} ${el.rect[2]}x${el.rect[3]}`;
  if (el.offscreen) s += ' (offscreen)';
  return s;
}

// The observation text: lines up to a character budget, then a note on how
// to reach the rest.
export function capLines(lines, maxChars = MAX_OBSERVATION_CHARS) {
  let used = 0;
  let i = 0;
  for (; i < lines.length; i++) {
    if (used + lines[i].length + 1 > maxChars) break;
    used += lines[i].length + 1;
  }
  const shown = lines.slice(0, i);
  if (i < lines.length) {
    shown.push(`# … ${lines.length - i} more elements not shown (${lines.length} in all): getAXState({query: "words"}) searches all of them, find({role, name}) locates one`);
  }
  return shown.join('\n');
}
