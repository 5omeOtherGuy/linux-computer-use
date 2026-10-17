// Local "expense portal" for the webform task: a paginated, searchable list of
// expense requests, a detail page per request, an approval form and a
// confirmation page. Plain node:http on loopback, no external resources.
// Every POST is recorded in <out>/submissions.json: accepted approvals in
// `submissions`, rejected posts in `rejected`.
import { createServer } from 'node:http';
import { writeFileSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { join } from 'node:path';

const PAGE_SIZE = 10;
export const COST_CENTRES = [
  ['CC-101', 'CC-101 Finance'],
  ['CC-118', 'CC-118 People Ops'],
  ['CC-204', 'CC-204 Field Ops'],
  ['CC-240', 'CC-240 Field Sales'],
  ['CC-310', 'CC-310 Engineering'],
  ['CC-402', 'CC-402 Marketing'],
];

// Newest first. The target (EXP-1047, Dana Kim, Lisbon) is on page 2 of the
// unfiltered list; Dana Kimura's Lisbon request and Dana Kim's Berlin request
// share a search with it.
const REQUESTS = [
  ['EXP-1062', 'Priya Natarajan', 'Munich', 'Customer onboarding', '2026-10-06', '2026-10-08', 687.4],
  ['EXP-1061', 'Tom Becker', 'Hamburg', 'Trade fair stand', '2026-10-05', '2026-10-07', 1240.0],
  ['EXP-1060', 'Dana Kimura', 'Lisbon', 'Partner conference', '2026-10-02', '2026-10-04', 529.9],
  ['EXP-1059', 'Lena Vogt', 'Vienna', 'Supplier audit', '2026-10-01', '2026-10-02', 318.25],
  ['EXP-1058', 'Marco Rossi', 'Milan', 'Site inspection', '2026-09-29', '2026-10-01', 455.6],
  ['EXP-1057', 'Aisha Rahman', 'Copenhagen', 'Team offsite', '2026-09-28', '2026-09-30', 902.15],
  ['EXP-1056', 'Jonas Weber', 'Zurich', 'Security training', '2026-09-27', '2026-09-28', 610.0],
  ['EXP-1055', 'Sofia Lindqvist', 'Stockholm', 'Recruiting event', '2026-09-25', '2026-09-26', 377.8],
  ['EXP-1054', 'Dana Kim', 'Berlin', 'Leadership training', '2026-09-24', '2026-09-25', 296.4],
  ['EXP-1053', 'Kofi Mensah', 'Brussels', 'Regulatory meeting', '2026-09-22', '2026-09-23', 341.0],
  ['EXP-1052', 'Hannah Schulz', 'Prague', 'Field service visit', '2026-09-20', '2026-09-22', 268.75],
  ['EXP-1051', 'Ravi Patel', 'Amsterdam', 'Customer workshop', '2026-09-18', '2026-09-19', 488.3],
  ['EXP-1050', 'Elena Petrova', 'Warsaw', 'Vendor negotiation', '2026-09-16', '2026-09-17', 255.1],
  ['EXP-1049', 'Lukas Meier', 'Basel', 'Equipment pickup', '2026-09-15', '2026-09-15', 142.6],
  ['EXP-1048', 'Chloe Martin', 'Paris', 'Press briefing', '2026-09-13', '2026-09-14', 533.2],
  ['EXP-1047', 'Dana Kim', 'Lisbon', 'Field team site survey', '2026-09-10', '2026-09-12', 438.5],
  ['EXP-1046', 'Noah Fischer', 'Cologne', 'Installation support', '2026-09-08', '2026-09-09', 219.9],
  ['EXP-1045', 'Mia Novak', 'Ljubljana', 'Partner kickoff', '2026-09-06', '2026-09-08', 401.35],
  ['EXP-1044', 'Oliver Brown', 'Dublin', 'Customer escalation', '2026-09-04', '2026-09-05', 612.0],
  ['EXP-1043', 'Yuki Tanaka', 'Madrid', 'Field team site survey', '2026-09-02', '2026-09-04', 470.45],
  ['EXP-1042', 'Sara Costa', 'Porto', 'Quarterly review', '2026-08-30', '2026-08-31', 198.0],
  ['EXP-1041', 'Daniel Kim', 'Lisbon', 'Customer demo', '2026-08-28', '2026-08-29', 384.7],
].map(([id, employee, city, purpose, from, to, amount]) => ({ id, employee, city, purpose, from, to, amount, status: 'Pending' }));

export const TARGET = { id: 'EXP-1047', note: 'Approved per policy 7.2', cost_centre: 'CC-204', amount: 412.8, receipt_checked: true };

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const eur = (n) => `${n.toFixed(2)} EUR`;
const page = (title, body) => `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>${esc(title)} - Expense Portal</title>
<style>body{font:15px/1.4 sans-serif;margin:0;color:#222}header{background:#24466b;color:#fff;padding:10px 24px}header a{color:#fff}
main{padding:16px 24px;max-width:1000px}table{border-collapse:collapse;width:100%}th,td{border-bottom:1px solid #ddd;padding:6px 8px;text-align:left}
label{display:block;margin-top:12px;font-weight:600}input[type=text],input[type=number],select{width:320px;padding:4px}.error{color:#a00}
.check label{display:inline;font-weight:600}button{margin-top:16px;padding:6px 16px}nav.pages a{margin-right:12px}</style></head>
<body><header><strong>Expense Portal</strong> &middot; <a href="/">Expense requests</a></header><main>${body}</main></body></html>`;

export async function startPortal(out) {
  const requests = REQUESTS.map((r) => ({ ...r }));
  const log = { submissions: [], rejected: [] };
  const save = () => writeFileSync(join(out, 'submissions.json'), JSON.stringify(log, null, 2));
  save();

  const listPage = (url) => {
    const q = (url.searchParams.get('q') ?? '').trim();
    const ql = q.toLowerCase();
    const hits = requests.filter((r) => !ql || [r.id, r.employee, r.city, r.purpose].some((f) => f.toLowerCase().includes(ql)));
    const pages = Math.max(1, Math.ceil(hits.length / PAGE_SIZE));
    const p = Math.min(pages, Math.max(1, Number(url.searchParams.get('page')) || 1));
    const rows = hits.slice((p - 1) * PAGE_SIZE, p * PAGE_SIZE).map((r) => `<tr><td><a href="/requests/${r.id}">${r.id}</a></td><td>${esc(r.employee)}</td>
<td>${esc(r.city)}</td><td>${esc(r.purpose)}</td><td>${r.from} to ${r.to}</td><td>${eur(r.amount)}</td><td>${r.status}</td></tr>`).join('\n');
    const link = (n, text) => `<a href="/?${new URLSearchParams({ ...(q ? { q } : {}), page: String(n) })}">${text}</a>`;
    return page('Expense requests', `<h1>Expense requests</h1>
<form role="search" action="/" method="get"><label for="q">Search requests</label>
<input type="text" id="q" name="q" value="${esc(q)}" placeholder="Employee, city, purpose or ID"> <button type="submit">Search</button></form>
<p>${hits.length} request${hits.length === 1 ? '' : 's'}${q ? ` matching &quot;${esc(q)}&quot;` : ''}</p>
<table><thead><tr><th>ID</th><th>Employee</th><th>Destination</th><th>Purpose</th><th>Travel dates</th><th>Requested</th><th>Status</th></tr></thead>
<tbody>${rows || '<tr><td colspan="7">No requests found.</td></tr>'}</tbody></table>
<nav class="pages" aria-label="Pagination">${p > 1 ? link(p - 1, 'Previous page') : ''} <span>Page ${p} of ${pages}</span> ${p < pages ? link(p + 1, 'Next page') : ''}</nav>`);
  };

  const detailPage = (r) => page(r.id, `<h1>Expense request ${r.id}</h1>
<dl><dt>Employee</dt><dd>${esc(r.employee)}</dd><dt>Destination</dt><dd>${esc(r.city)}</dd><dt>Purpose</dt><dd>${esc(r.purpose)}</dd>
<dt>Travel dates</dt><dd>${r.from} to ${r.to}</dd><dt>Requested amount</dt><dd>${eur(r.amount)}</dd><dt>Status</dt><dd>${r.status}</dd></dl>
<p>${r.status === 'Pending' ? `<a href="/requests/${r.id}/approve">Review and approve</a> &middot; ` : ''}<a href="/">Back to expense requests</a></p>`);

  const formPage = (r, errors = [], v = {}) => page(`Approve ${r.id}`, `<h1>Approve expense request ${r.id}</h1>
<p>${esc(r.employee)} &middot; ${esc(r.city)} &middot; ${esc(r.purpose)} &middot; requested ${eur(r.amount)}</p>
${errors.length ? `<div class="error" role="alert"><p>Please correct the following:</p><ul>${errors.map((e) => `<li>${esc(e)}</li>`).join('')}</ul></div>` : ''}
<form method="post" action="/requests/${r.id}/approve">
<label for="note">Approval note</label><input type="text" id="note" name="note" value="${esc(v.note ?? '')}">
<label for="cost_centre">Cost centre</label><select id="cost_centre" name="cost_centre"><option value="">Select a cost centre</option>
${COST_CENTRES.map(([val, text]) => `<option value="${val}"${v.cost_centre === val ? ' selected' : ''}>${text}</option>`).join('')}</select>
<label for="amount">Approved amount (EUR)</label><input type="number" id="amount" name="amount" step="0.01" min="0" value="${esc(v.amount ?? r.amount.toFixed(2))}">
<p class="check"><input type="checkbox" id="receipt" name="receipt_checked" value="yes"${v.receipt_checked ? ' checked' : ''}> <label for="receipt">Receipt checked</label></p>
<button type="submit">Submit approval</button></form>
<p><a href="/requests/${r.id}">Cancel</a></p>`);

  let loaded;
  const firstLoad = new Promise((ok) => { loaded = ok; });
  const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://portal.invalid');
    const send = (code, html) => {
      res.writeHead(code, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      res.end(html, loaded);
    };
    const m = url.pathname.match(/^\/requests\/(EXP-\d+)(\/approve)?$/);
    const r = m && requests.find((x) => x.id === m[1]);
    if (req.method === 'GET' && url.pathname === '/') return send(200, listPage(url));
    if (req.method === 'GET' && r && !m[2]) return send(200, detailPage(r));
    if (req.method === 'GET' && r && m[2]) {
      return r.status === 'Pending' ? send(200, formPage(r)) : send(409, page('Already decided', `<h1>${r.id} is already ${r.status.toLowerCase()}</h1><p><a href="/">Back to expense requests</a></p>`));
    }
    if (req.method === 'POST' && r && m[2]) {
      let body = '';
      req.on('data', (d) => { body += d; });
      req.on('end', () => {
        const f = new URLSearchParams(body);
        const v = { note: f.get('note') ?? '', cost_centre: f.get('cost_centre') ?? '', amount: f.get('amount') ?? '', receipt_checked: f.get('receipt_checked') === 'yes' };
        const amount = Number(v.amount);
        const errors = [];
        if (r.status !== 'Pending') errors.push(`${r.id} is already ${r.status.toLowerCase()}.`);
        if (!v.note.trim()) errors.push('Enter an approval note.');
        if (!COST_CENTRES.some(([val]) => val === v.cost_centre)) errors.push('Select a cost centre.');
        if (!v.amount.trim() || !Number.isFinite(amount) || amount <= 0 || amount > r.amount) errors.push(`Enter an approved amount between 0.01 and ${r.amount.toFixed(2)} EUR.`);
        const at = new Date().toISOString();
        if (errors.length) {
          log.rejected.push({ at, request_id: r.id, ...v, errors });
          save();
          return send(422, formPage(r, errors, v));
        }
        const confirmation = `APR-${randomBytes(3).toString('hex').toUpperCase()}`;
        r.status = 'Approved';
        log.submissions.push({ at, request_id: r.id, ...v, amount_value: amount, confirmation });
        save();
        send(200, page('Approval recorded', `<h1>Approval recorded</h1><p role="status">Confirmation number: <strong>${confirmation}</strong></p>
<p>${r.id} (${esc(r.employee)}, ${esc(r.city)}) approved for ${eur(amount)} on ${esc(COST_CENTRES.find(([val]) => val === v.cost_centre)[1])}.</p>
<p><a href="/">Back to expense requests</a></p>`));
      });
      return undefined;
    }
    return send(404, page('Not found', '<h1>Not found</h1><p><a href="/">Back to expense requests</a></p>'));
  });
  await new Promise((ok, fail) => server.once('error', fail).listen(0, '127.0.0.1', ok));
  // firstLoad: the browser has received its first page (the harness waits for it during setup).
  return { url: `http://127.0.0.1:${server.address().port}/`, firstLoad, close:() => new Promise((ok) => { server.closeAllConnections(); server.close(ok); }) };
}
