// localmail page script: copy buttons everywhere, the live inbox and message viewer, and the
// attempts log. Everything goes through the same /dev/api the tests use.
'use strict';

const h = (tag, attrs = {}, ...children) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.className = v;
    else if (k === 'style') el.setAttribute('style', v);
    else if (k === 'value') el.value = v;
    else if (k in el && typeof v !== 'string') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat(Infinity)) if (c != null) el.append(c.nodeType ? c : document.createTextNode(c));
  return el;
};

const copied = (btn) => {
  const old = btn.textContent; btn.textContent = 'Copied'; setTimeout(() => (btn.textContent = old), 1200);
};

function copyBtn(text, label = 'Copy') {
  return h('button', { type: 'button', class: 'btn small', onclick: async (e) => {
    // currentTarget is cleared once the event has been dispatched, i.e. after the await.
    const btn = e.currentTarget;
    await navigator.clipboard.writeText(text);
    copied(btn);
  } }, label);
}

document.addEventListener('click', async (e) => {
  const btn = e.target.closest('[data-copy-next]');
  if (!btn) return;
  const target = btn.closest('.snippet')?.querySelector('pre') || btn.nextElementSibling;
  if (!target) return;
  await navigator.clipboard.writeText(target.textContent);
  copied(btn);
});

async function api(path, opts = {}) {
  const res = await fetch(path, { headers: { accept: 'application/json' }, ...opts });
  if (res.status === 204) return null;
  const body = await res.json().catch(() => null);
  if (!res.ok && res.status !== 408) throw new Error(body?.error_description || `HTTP ${res.status}`);
  return body;
}

const pad = (n) => String(n).padStart(2, '0');
function when(ms) {
  const d = new Date(ms);
  const now = new Date();
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (d.toDateString() === now.toDateString()) return time;
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${time}`;
}
const fullTime = (ms) => new Date(ms).toLocaleString();
const addr = (a) => (a ? (a.name ? `${a.name} <${a.email}>` : a.email) : '');
const size = (n) => (n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(1)} MB`);

// Pages embed their data (the providers, and for the inbox its addresses) as JSON.
const pageData = JSON.parse(document.getElementById('lm-data')?.textContent || '{}');
const providers = Object.fromEntries((pageData.providers || []).map((p) => [p.id, p]));
function providerChip(id) {
  const p = providers[id] || { name: id, color: '#64748b' };
  return h('span', { class: 'provider-chip', style: `--c:${p.color}` }, p.name);
}

// ------------------------------------------------------------------ inbox
(function () {
  const root = document.getElementById('lm-inbox');
  if (!root) return;
  const data = pageData;
  const list = document.getElementById('lm-messages');
  const view = document.getElementById('lm-view');
  const search = document.getElementById('lm-search');
  const providerSel = document.getElementById('lm-provider');
  const count = document.getElementById('lm-count');
  for (const p of data.providers) providerSel.append(h('option', { value: p.id }, p.name));

  const state = { messages: [], selected: null, cursor: 0, tab: 'html' };

  const selectedId = () => {
    const m = location.hash.match(/^#\/m\/(\d+)/);
    return m ? Number(m[1]) : null;
  };

  async function load() {
    const q = new URLSearchParams({ limit: '300' });
    if (search.value.trim()) q.set('q', search.value.trim());
    if (providerSel.value) q.set('provider', providerSel.value);
    const res = await api(`/dev/api/messages?${q}`);
    state.messages = res.messages;
    state.cursor = res.cursor;
    const filtered = search.value.trim() || providerSel.value;
    count.textContent = filtered ? `${res.total} matching` : `${res.total} message${res.total === 1 ? '' : 's'}, ${res.unread} unread`;
    document.title = `${res.unread ? `(${res.unread}) ` : ''}Inbox · localmail`;
    renderList();
  }

  function renderList() {
    list.replaceChildren();
    if (!state.messages.length) {
      const filtered = search.value.trim() || providerSel.value;
      list.append(h('li', { class: 'empty' }, filtered ? 'No messages match.' : [
        'No messages yet. Send over SMTP to ', h('code', {}, data.smtp), ' or call a provider API at ', h('code', {}, data.base),
        '. ', h('a', { href: '/setup' }, 'Setup'), ' shows how.',
      ]));
      return;
    }
    for (const m of state.messages) {
      const to = [...m.to, ...m.cc].map((a) => a.email).join(', ') || m.bcc.map((a) => a.email).join(', ');
      const li = h('li', {
        class: `msg${m.read ? '' : ' unread'}${m.id === state.selected ? ' selected' : ''}`,
        tabindex: '0',
        'data-id': m.id,
        onclick: () => open(m.id),
        onkeydown: (e) => { if (e.key === 'Enter') open(m.id); },
      },
      h('div', { class: 'msg-top' }, h('span', { class: 'msg-from' }, m.from ? m.from.name || m.from.email : '(no sender)'), h('span', { class: 'msg-time', title: fullTime(m.received_at) }, when(m.received_at))),
      h('div', { class: 'msg-subject' }, m.subject || (m.template ? `(template ${m.template.id})` : '(no subject)')),
      h('div', { class: 'msg-to' }, `To ${to}`),
      m.preview ? h('div', { class: 'msg-preview' }, m.preview) : null,
      h('div', { class: 'msg-meta' }, providerChip(m.provider),
        m.has_attachments ? h('span', { class: 'chip muted', title: 'Has attachments' }, '📎') : null,
        m.scenario ? h('span', { class: 'chip warn' }, m.scenario.split('@')[0]) : null,
        m.template ? h('span', { class: 'chip muted' }, 'template') : null));
      list.append(li);
    }
  }

  function open(id) {
    if (selectedId() !== id) location.hash = `#/m/${id}`;
    else show(id);
  }

  function step(delta) {
    const i = state.messages.findIndex((m) => m.id === state.selected);
    const next = state.messages[i === -1 ? 0 : i + delta];
    if (next) {
      open(next.id);
      list.querySelector(`[data-id="${next.id}"]`)?.scrollIntoView({ block: 'nearest' });
    }
  }

  function empty() {
    view.replaceChildren(h('div', { class: 'view-empty' }, h('div', { class: 'logo' }, '✉'),
      h('p', {}, 'Pick a message to read it.'),
      h('p', { class: 'small' }, 'j / k move through the list.')));
  }

  async function show(id) {
    state.selected = id;
    for (const li of list.querySelectorAll('.msg')) li.classList.toggle('selected', Number(li.dataset.id) === id);
    let m;
    try {
      m = await api(`/dev/api/messages/${id}`);
    } catch (e) {
      view.replaceChildren(h('div', { class: 'view-empty' }, h('p', {}, `Message ${id} isn't here any more.`)));
      return;
    }
    if (!m.read) {
      api(`/dev/api/messages/${id}/read`, { method: 'POST' }).then(() => {
        const s = state.messages.find((x) => x.id === id);
        if (s) { s.read = true; list.querySelector(`[data-id="${id}"]`)?.classList.remove('unread'); }
        load();
      });
    }
    renderMessage(m);
  }

  function renderMessage(m) {
    const rows = [['From', addr(m.from)], ['To', m.to.map(addr).join(', ')], ['Cc', m.cc.map(addr).join(', ')],
      ['Bcc', m.bcc.map(addr).join(', ')], ['Reply-To', m.reply_to.map(addr).join(', ')], ['Date', fullTime(m.received_at)]]
      .filter(([, v]) => v);
    const head = h('div', { class: 'view-head' },
      h('div', { class: 'view-title' },
        h('h1', {}, m.subject || (m.template ? `(template ${m.template.id})` : '(no subject)')),
        h('div', { class: 'view-actions' },
          h('a', { class: 'btn small', href: `/dev/api/messages/${m.id}/raw?download` }, 'Download .eml'),
          h('button', { class: 'btn small danger', type: 'button', onclick: async () => {
            await api(`/dev/api/messages/${m.id}`, { method: 'DELETE' });
            const i = state.messages.findIndex((x) => x.id === m.id);
            const next = state.messages[i + 1] || state.messages[i - 1];
            await load();
            if (next) open(next.id); else { location.hash = ''; empty(); }
          } }, 'Delete'))),
      h('dl', { class: 'addresses' }, rows.flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)])),
      h('div', { class: 'view-chips' }, providerChip(m.provider), h('code', { title: 'Endpoint' }, m.endpoint),
        h('span', { class: 'chip muted', title: 'The message ID the provider API returned' }, m.provider_message_id),
        ...m.tags.map((t) => h('span', { class: 'chip' }, t)),
        m.scenario ? h('span', { class: 'chip warn', title: 'Scenario address' }, m.scenario) : null));

    const parts = [head];
    if (m.template && !m.template.rendered) {
      parts.push(h('div', { class: 'note' }, 'Sent with the template ', h('code', {}, `${m.template.field} = ${m.template.id}`),
        ', which lives in the provider account. localmail shows its data instead of rendering it.'));
    }
    const found = h('div', { class: 'found', hidden: true });
    parts.push(found);
    api(`/dev/api/messages/${m.id}/extract?extract=otp,links`).then((x) => {
      const items = [];
      if (x.otp) items.push(h('span', { class: 'item' }, h('b', {}, 'Code'), h('code', { class: 'otp' }, x.otp), copyBtn(x.otp)));
      for (const l of x.links.slice(0, 3)) {
        items.push(h('span', { class: 'item' }, h('b', {}, 'Link'), h('a', { href: l.href, target: '_blank', rel: 'noopener', title: l.href }, l.text || l.href), copyBtn(l.href)));
      }
      if (x.links.length > 3) items.push(h('span', { class: 'muted small' }, `+${x.links.length - 3} more links`));
      if (items.length) { found.replaceChildren(...items); found.hidden = false; }
    }).catch(() => {});

    if (m.attachments.length) {
      parts.push(h('div', { class: 'attachments' }, m.attachments.map((a) => h('a', {
        class: 'attachment', href: `/dev/api/messages/${m.id}/attachments/${a.index}?download`,
      }, '📎 ', a.filename, h('small', {}, `${a.content_type} · ${size(a.size)}${a.content_id ? ' · inline' : ''}`)))));
    }

    const tabs = [
      m.html != null && ['html', 'HTML'],
      m.text != null && ['text', 'Text'],
      ['headers', 'Headers'],
      ['source', 'Source'],
      m.request && ['request', 'Request'],
      ['details', 'Details'],
    ].filter(Boolean);
    if (!tabs.some(([id]) => id === state.tab)) state.tab = tabs[0][0];
    const panel = h('div', { class: 'tab-panel' });
    const bar = h('div', { class: 'tabs', role: 'tablist' }, tabs.map(([id, label]) => h('button', {
      type: 'button', class: 'tab', role: 'tab', 'aria-selected': String(id === state.tab),
      onclick: (e) => {
        state.tab = id;
        for (const b of bar.children) b.setAttribute('aria-selected', String(b === e.currentTarget));
        renderTab(m, panel);
      },
    }, label)));
    parts.push(bar, panel);
    renderTab(m, panel);
    view.replaceChildren(...parts);
    view.scrollTop = 0;
  }

  function renderTab(m, panel) {
    switch (state.tab) {
      case 'html': {
        const frame = h('iframe', {
          class: 'html-frame', src: `/dev/api/messages/${m.id}/html`, title: 'HTML body',
          sandbox: 'allow-popups allow-popups-to-escape-sandbox', referrerpolicy: 'no-referrer',
        });
        panel.replaceChildren(frame);
        break;
      }
      case 'text':
        panel.replaceChildren(h('pre', { class: 'code text-body' }, m.text));
        break;
      case 'headers':
        panel.replaceChildren(h('table', { class: 'headers-table' }, m.headers.map((x) => h('tr', {}, h('td', {}, x.name), h('td', {}, x.value)))));
        break;
      case 'source': {
        const pre = h('pre', { class: 'code' }, 'Loading…');
        panel.replaceChildren(h('div', { class: 'snippet' }, h('div', { class: 'token-head' }, h('b', {}, 'MIME source'),
          h('button', { type: 'button', class: 'btn small', 'data-copy-next': '' }, 'Copy')), pre));
        fetch(`/dev/api/messages/${m.id}/raw`).then((r) => r.text()).then((t) => (pre.textContent = t));
        break;
      }
      case 'request': {
        const text = typeof m.request === 'string' ? m.request : JSON.stringify(m.request, null, 2);
        panel.replaceChildren(h('p', { class: 'muted small' }, `What your app sent to ${m.endpoint} (long values such as attachment contents are shortened).`),
          h('div', { class: 'snippet' }, h('div', { class: 'token-head' }, h('b', {}, 'Request'), h('button', { type: 'button', class: 'btn small', 'data-copy-next': '' }, 'Copy')),
            h('pre', { class: 'code' }, text)));
        break;
      }
      case 'details': {
        const rows = [['localmail id', String(m.id)], ['Provider message ID', m.provider_message_id], ['Endpoint', m.endpoint],
          ['Message-ID', m.message_id || ''], ['Size', size(m.size)]];
        if (m.template) rows.push(['Template', `${m.template.field} = ${m.template.id}`], ['Template data', JSON.stringify(m.template.data, null, 2)]);
        for (const [k, v] of Object.entries(m.details || {})) rows.push([k, typeof v === 'string' ? v : JSON.stringify(v, null, 2)]);
        if (m.envelope) {
          rows.push(['MAIL FROM', m.envelope.mail_from || '<>'], ['RCPT TO', m.envelope.rcpt_to.join('\n')]);
          if (m.envelope.helo) rows.push(['EHLO', m.envelope.helo]);
          if (m.envelope.auth_user) rows.push(['AUTH user', m.envelope.auth_user]);
        }
        panel.replaceChildren(h('dl', { class: 'kv details' }, rows.filter(([, v]) => v !== '').flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)])));
        break;
      }
    }
  }

  // Wake up whenever a message arrives: the same long-poll tests use.
  async function watch() {
    for (;;) {
      try {
        const res = await api(`/dev/api/messages/wait?after=${state.cursor}&timeout=25s`);
        if (res && res.message) await load();
        else if (res && res.cursor < state.cursor) await load(); // restarted with an empty inbox
      } catch {
        // The connection dropped, usually a restart: once the server is back, reload, which also
        // picks up its cursor (back to 0 after an --in-memory restart).
        await new Promise((r) => setTimeout(r, 1000));
        await load().catch(() => {});
      }
    }
  }

  let timer;
  search.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(load, 200); });
  providerSel.addEventListener('change', load);
  document.getElementById('lm-read-all').addEventListener('click', async () => { await api('/dev/api/messages/read', { method: 'POST' }); load(); });
  document.getElementById('lm-delete-all').addEventListener('click', async () => {
    if (!confirm('Delete every message?')) return;
    await api('/dev/api/messages', { method: 'DELETE' });
    location.hash = '';
    empty();
    load();
  });
  document.addEventListener('keydown', (e) => {
    if (e.target.matches('input, select, textarea') || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === 'j') step(1);
    if (e.key === 'k') step(-1);
  });
  window.addEventListener('hashchange', () => { const id = selectedId(); if (id) show(id); else empty(); });

  load().then(() => {
    const id = selectedId();
    if (id) show(id); else empty();
    watch();
  });
})();

// ------------------------------------------------------------------ attempts
(function () {
  const root = document.getElementById('lm-attempts');
  if (!root) return;
  const providerSel = document.getElementById('att-provider');
  const errors = document.getElementById('att-errors');
  const pause = document.getElementById('att-pause');
  let entries = [];
  let latest = 0;
  let paused = false;
  const open = new Set();

  async function poll() {
    if (paused) return;
    try {
      const res = await api(`/dev/api/attempts?after=${latest}`);
      if (res.latest_id < latest) { entries = []; latest = 0; return poll(); }
      if (res.attempts.length) {
        entries = [...res.attempts, ...entries].slice(0, 1000);
        render();
      } else if (!entries.length) render();
      latest = res.latest_id;
    } catch { /* try again next tick */ }
  }

  function render() {
    const shown = entries.filter((a) => (!providerSel.value || a.provider === providerSel.value) && (!errors.checked || a.outcome !== 'accepted'));
    if (!shown.length) {
      root.replaceChildren(h('p', { class: 'muted req-empty' }, entries.length ? 'No attempts match these filters.' : 'No sends yet.'));
      return;
    }
    const rows = [];
    for (const a of shown) {
      const bad = a.status >= 400;
      rows.push(h('tr', {
        class: `req-row${a.outcome === 'failed' ? ' failed' : ''}${a.outcome === 'partial' ? ' partial' : ''}${open.has(a.id) ? ' open' : ''}`,
        tabindex: '0',
        onclick: () => { open.has(a.id) ? open.delete(a.id) : open.add(a.id); render(); },
        onkeydown: (e) => { if (e.key === 'Enter') e.currentTarget.click(); },
      },
      h('td', { class: 'mono', title: fullTime(a.at) }, new Date(a.at).toLocaleTimeString()),
      h('td', {}, providerChip(a.provider)),
      h('td', { class: 'mono' }, a.endpoint),
      h('td', {}, h('span', { class: `status ${bad ? 'bad' : 'ok'}` }, String(a.status)), a.outcome === 'partial' ? h('span', { class: 'chip warn' }, 'partial') : null),
      h('td', {}, a.recipients.join(', ')),
      h('td', {}, a.error ? h('span', { class: 'req-error' }, a.error) : a.messages.length ? `${a.messages.length} stored` : '',
        a.scenario ? h('span', { class: 'chip warn' }, a.scenario.split('@')[0]) : null)));
      if (open.has(a.id)) {
        const details = [['Subject', a.subject], ['From', a.from], ['Error', a.error_description], ['Warnings', a.warnings?.join('\n')],
          ['Duration', `${a.duration_ms} ms`]].filter(([, v]) => v);
        rows.push(h('tr', { class: 'req-detail' }, h('td', { colspan: '6' },
          h('dl', { class: 'req-details' }, details.flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]),
            a.messages.length ? [h('dt', {}, 'Messages'), h('dd', {}, a.messages.map((id, i) => [i ? ', ' : '', h('a', { href: `/#/m/${id}` }, `#${id}`)]))] : []),
          a.request ? [h('div', { class: 'section-title' }, a.provider === 'smtp' ? 'Transcript' : 'Request'), h('pre', { class: 'code req-pre' }, a.request)] : null,
          a.response ? [h('div', { class: 'section-title' }, 'Response'), h('pre', { class: 'code req-pre' }, a.response)] : null)));
      }
    }
    root.replaceChildren(h('table', { class: 'req-table' },
      h('thead', {}, h('tr', {}, ['Time', 'Provider', 'Endpoint', 'Status', 'Recipients', 'Outcome'].map((t) => h('th', {}, t)))),
      h('tbody', {}, rows)));
  }

  providerSel.addEventListener('change', render);
  errors.addEventListener('change', render);
  pause.addEventListener('click', () => { paused = !paused; pause.textContent = paused ? 'Resume' : 'Pause'; if (!paused) poll(); });
  document.getElementById('att-clear').addEventListener('click', async () => {
    await api('/dev/api/attempts', { method: 'DELETE' });
    entries = [];
    render();
  });
  poll();
  setInterval(poll, 1500);
})();
