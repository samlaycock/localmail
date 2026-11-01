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
  const mailboxList = document.getElementById('lm-mailboxes');
  const mailboxFilter = document.getElementById('lm-mbox-filter');
  const mailboxHead = document.getElementById('lm-mailbox-head');
  for (const p of data.providers) providerSel.append(h('option', { value: p.id }, p.name));

  // `mailbox`: the recipient address being shown (`?to=` in the URL), or null for all mail.
  const state = { messages: [], total: 0, selected: null, cursor: 0, tab: 'html', mailbox: null };
  const mailboxFromUrl = () => new URLSearchParams(location.search).get('to')?.trim().toLowerCase() || null;
  const mailboxUrl = (address) => (address ? `/?to=${encodeURIComponent(address)}` : '/');
  state.mailbox = mailboxFromUrl();

  /** Show one address's mail (or all mail with null). */
  function setMailbox(address) {
    address = address ? address.trim().toLowerCase() : null;
    if (address === state.mailbox) return;
    state.mailbox = address;
    history.pushState(null, '', mailboxUrl(address));
    state.selected = null;
    empty();
    load();
  }

  /** A link to an address's mailbox that opens it in place (and still works in a new tab). */
  const mailboxLink = (address, label, cls = 'addr-link') => h('a', {
    class: cls, href: mailboxUrl(address), title: `Mail to ${address}`,
    onclick: (e) => { if (e.metaKey || e.ctrlKey || e.shiftKey || e.button) return; e.preventDefault(); setMailbox(address); },
  }, label);

  const selectedId = () => {
    const m = location.hash.match(/^#\/m\/(\d+)/);
    return m ? Number(m[1]) : null;
  };

  /** What the list shows, as API filters: the open mailbox, the search and the provider. The bulk
   * actions use the same filters, so they act on exactly the messages listed. */
  function listFilters() {
    const q = new URLSearchParams(state.mailbox ? { to: state.mailbox } : {});
    if (search.value.trim()) q.set('q', search.value.trim());
    if (providerSel.value) q.set('provider', providerSel.value);
    return q;
  }

  // Loads overlap (live updates, typing, switching mailboxes); only the latest one may render.
  let loads = 0;
  async function load() {
    const seq = ++loads;
    const q = listFilters();
    q.set('limit', '300');
    const mq = new URLSearchParams({ limit: '200' });
    if (mailboxFilter.value.trim()) mq.set('q', mailboxFilter.value.trim());
    const [res, boxes] = await Promise.all([api(`/dev/api/messages?${q}`), api(`/dev/api/mailboxes?${mq}`)]);
    if (seq !== loads) return;
    state.messages = res.messages;
    state.total = res.total;
    state.cursor = res.cursor;
    const filtered = search.value.trim() || providerSel.value;
    count.textContent = filtered ? `${res.total} matching` : `${res.total} message${res.total === 1 ? '' : 's'}, ${res.unread} unread`;
    // In a mailbox, its own unread count (the list's, unless a search narrows the list).
    const open = state.mailbox && boxes.mailboxes.find((b) => b.address === state.mailbox);
    const unread = !state.mailbox ? boxes.unread : open ? open.unread : filtered ? null : res.unread;
    document.title = `${unread ? `(${unread}) ` : ''}${state.mailbox || 'Inbox'} · localmail`;
    renderMailboxes(boxes);
    renderList();
  }

  function renderMailboxes(boxes) {
    // `total` is null when unknown (the open mailbox, hidden by the filter or the limit).
    const item = (address, label, total, unread) => {
      const current = (address || null) === state.mailbox;
      const badge = unread ? h('span', { class: 'mbox-unread', title: `${unread} unread` }, String(unread))
        : total != null ? h('span', { class: 'mbox-total', title: `${total} messages` }, String(total)) : null;
      const link = mailboxLink(address, [h('span', { class: 'mbox-name' }, label), badge], `mbox${current ? ' current' : ''}`);
      if (current) link.setAttribute('aria-current', 'page');
      return h('li', {}, link);
    };
    const items = [item(null, 'All mail', boxes.messages, boxes.unread)];
    // The open mailbox stays listed even if the filter (or the limit) would hide it.
    if (state.mailbox && !boxes.mailboxes.some((b) => b.address === state.mailbox)) items.push(item(state.mailbox, state.mailbox, null, null));
    for (const b of boxes.mailboxes) items.push(item(b.address, b.address, b.total, b.unread));
    if (boxes.total > boxes.mailboxes.length) {
      items.push(h('li', { class: 'mbox-more muted small' }, `Showing ${boxes.mailboxes.length} of ${boxes.total}. Filter to find others.`));
    } else if (!boxes.mailboxes.length && mailboxFilter.value.trim()) {
      items.push(h('li', { class: 'mbox-more muted small' }, 'No addresses match.'));
    }
    mailboxList.replaceChildren(...items);

    if (state.mailbox) {
      const link = location.origin + mailboxUrl(state.mailbox);
      mailboxHead.replaceChildren(
        h('div', { class: 'mailbox-current' }, h('span', { class: 'muted small' }, 'Mail to'), h('b', {}, state.mailbox)),
        copyBtn(link, 'Copy link'),
        h('button', { type: 'button', class: 'btn small ghost', title: 'Show all mail', onclick: () => setMailbox(null) }, 'All mail'));
      mailboxHead.hidden = false;
    } else {
      mailboxHead.hidden = true;
    }
  }

  function renderList() {
    list.replaceChildren();
    if (!state.messages.length) {
      const filtered = search.value.trim() || providerSel.value;
      list.append(h('li', { class: 'empty' }, filtered ? 'No messages match.' : state.mailbox ? [
        'No mail to ', h('code', {}, state.mailbox), ' yet. It will appear here as soon as it arrives.',
      ] : [
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
    // Recipients link to their mailboxes.
    const recipients = (list) => list.flatMap((a, i) => [i ? ', ' : '', mailboxLink(a.email, addr(a))]);
    const rows = [['From', addr(m.from)], ['To', recipients(m.to)], ['Cc', recipients(m.cc)],
      ['Bcc', recipients(m.bcc)], ['Reply-To', m.reply_to.map(addr).join(', ')], ['Date', fullTime(m.received_at)]]
      .filter(([, v]) => v.length);
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
  let mboxTimer;
  mailboxFilter.addEventListener('input', () => { clearTimeout(mboxTimer); mboxTimer = setTimeout(load, 200); });
  // Bulk actions apply to the messages listed: the open mailbox, narrowed by the search and provider.
  document.getElementById('lm-read-all').addEventListener('click', async () => {
    await api(`/dev/api/messages/read?${listFilters()}`, { method: 'POST' });
    load();
  });
  /** "Delete every message?", or what exactly will go, e.g. "Delete the 2 messages to a@b.test matching "code"?" */
  function deleteQuestion() {
    const searchText = search.value.trim();
    const provider = providerSel.value && providers[providerSel.value]?.name;
    if (!state.mailbox && !searchText && !provider) return 'Delete every message?';
    const n = state.total;
    return `Delete ${n === 1 ? 'the 1 message' : `the ${n} messages`}${state.mailbox ? ` to ${state.mailbox}` : ''}`
      + `${searchText ? ` matching “${searchText}”` : ''}${provider ? ` sent through ${provider}` : ''}?`;
  }
  document.getElementById('lm-delete-all').addEventListener('click', async () => {
    if (!state.total) return;
    if (!confirm(deleteQuestion())) return;
    await api(`/dev/api/messages?${listFilters()}`, { method: 'DELETE' });
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
  // Back and forward between mailboxes.
  window.addEventListener('popstate', () => {
    const mailbox = mailboxFromUrl();
    if (mailbox === state.mailbox) return;
    state.mailbox = mailbox;
    load().then(() => { const id = selectedId(); if (id) show(id); else empty(); });
  });

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
