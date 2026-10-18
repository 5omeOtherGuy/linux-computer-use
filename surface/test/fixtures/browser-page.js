// A local web app for browser tests: a form whose submission navigates to a
// result page, plus a list of ?items=N rows (default 100: about 300
// accessible elements in all).
import http from 'node:http';

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

export function formPage(items = 100) {
  const rows = Array.from({ length: items }, (_, i) => `<li>Item ${i + 1} <a href="/item/${i + 1}">open item ${i + 1}</a> <span>updated ${i + 1} days ago</span></li>`).join('\n');
  return `<!doctype html><html><head><title>Order form</title></head><body>
<header><nav aria-label="Main"><a href="/">Home</a> <a href="/help">Help</a></nav></header>
<main>
<h1>Place an order</h1>
<form action="/result" method="get">
  <p><label for="name">Name</label> <input id="name" name="name" value="old"></p>
  <p><label for="color">Color</label> <select id="color" name="color"><option value="r">Red</option><option value="g">Green</option><option value="b">Blue</option></select></p>
  <p><label><input type="checkbox" name="gift" value="yes"> Gift wrap</label></p>
  <p><label for="note">Note</label> <textarea id="note" name="note"></textarea></p>
  <p><button type="submit">Send order</button> <button type="button" id="later" onclick="document.getElementById('status').textContent='Saved for later'">Save for later</button></p>
  <p id="status" role="status"></p>
  <p><label for="code">Code</label> <input id="code" oninput="this.value = this.value.toUpperCase()"></p>
  <div style="position: relative; display: inline-block"><button type="button">Covered</button><div style="position: absolute; inset: 0; background: white"></div></div>
</form>
<h2>Recent items</h2>
<ul>${rows}</ul>
</main>
<div id="overlay" style="display:none;position:fixed;inset:0;background:#0008"></div>
</body></html>`;
}

export function startServer() {
  const server = http.createServer((req, res) => {
    const u = new URL(req.url, 'http://x');
    res.setHeader('content-type', 'text/html; charset=utf-8');
    if (u.pathname === '/result') {
      const p = u.searchParams;
      res.end(`<!doctype html><title>Order received</title><main><h1>Thank you, ${esc(p.get('name'))}</h1><p id="summary">Color ${esc(p.get('color'))}, gift ${esc(p.get('gift') ?? 'no')}, note ${esc(p.get('note'))}</p><a href="/">Back</a></main>`);
      return;
    }
    res.end(formPage(Number(u.searchParams.get('items') ?? 100)));
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, url: `http://127.0.0.1:${server.address().port}/` })));
}
