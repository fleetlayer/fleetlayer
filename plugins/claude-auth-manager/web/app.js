// The hub's page: accounts, and the sign-in and setup-token flows. Plain JS, no build step, nothing kept in the
// browser: sign-in links and codes live only in this page's memory while a flow is open.
'use strict';

const $ = (s, el = document) => el.querySelector(s);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const ICONS = {
  more: '<circle cx="5" cy="12" r="1.1"/><circle cx="12" cy="12" r="1.1"/><circle cx="19" cy="12" r="1.1"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  check: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
  external: '<path d="M14 5h5v5M19 5l-8 8M18 14v4a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h4"/>',
  alert: '<circle cx="12" cy="12" r="9"/><path d="M12 7.5v5.5M12 16.5v.01"/>',
  login: '<path d="M10 17l5-5-5-5M15 12H4M14 4h5a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-5"/>',
  key: '<circle cx="8" cy="15" r="3.5"/><path d="M10.5 12.5L19 4M16 7l2.5 2.5"/>',
  trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 12a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-12M9 7V4h6v3"/>',
  users: '<circle cx="9" cy="8.5" r="3.5"/><path d="M2.5 19.5c1-3 3.4-4.8 6.5-4.8s5.5 1.8 6.5 4.8M16 5.2a3.5 3.5 0 0 1 0 6.6M18.5 14.9c1.4.8 2.4 2.4 3 4.6"/>',
  play: '<path d="M8 5.5v13l10-6.5z"/>',
};
const icon = (name) => `<svg class="i" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name]}</svg>`;

class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

async function api(path, opts = {}) {
  let r;
  try {
    r = await fetch(path, {
      method: opts.method || 'GET',
      credentials: 'same-origin',
      headers: opts.body ? { 'content-type': 'application/json' } : undefined,
      body: opts.body ? JSON.stringify(opts.body) : undefined,
    });
  } catch {
    throw new ApiError("Can't reach the hub.", 0);
  }
  if (r.status === 401) {
    location.assign('/login');
    throw new ApiError('Signed out of the hub.', 401);
  }
  let body = null;
  try { body = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) throw new ApiError((body && body.error) || `The hub answered ${r.status}.`, r.status);
  return body;
}

// ------------------------------------------------------------------ formatting and status

const dateFmt = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
const fmtDate = (iso) => (iso ? dateFmt.format(new Date(iso)) : '');
const days = (n) => `${n} day${n === 1 ? '' : 's'}`;
const cap = (s) => (s ? s[0].toUpperCase() + s.slice(1) : s);

function tierLabel(tier) {
  if (!tier) return '';
  const t = tier.toLowerCase();
  const m = t.match(/max_(\d+)x/);
  if (m) return `Max ${m[1]}×`;
  if (t.includes('ultra')) return 'Ultra';
  if (t.includes('max')) return 'Max';
  if (t.includes('pro')) return 'Pro';
  return '';
}

// Why the last poll found a sign-in no longer works, in a few words (the hub's own wording as a fallback).
const NEEDED = {
  'the usage endpoint refused the sign-in': 'Claude refused it',
  "the sign-in couldn't be refreshed": "Couldn't refresh it",
  'no sign-in stored': 'Missing on the hub',
};

function signInStatus(a) {
  if (!a.signedInAt) return { tone: 'off', text: 'Not signed in' };
  if (a.signInNeeded) return { tone: 'bad', text: 'Sign in again', sub: NEEDED[a.signInNeeded] || cap(a.signInNeeded) };
  const d = a.signInDaysLeft;
  if (d != null && d <= 0) return { tone: 'bad', text: 'Expired', sub: a.signInExpiresAt && `on ${fmtDate(a.signInExpiresAt)}` };
  return {
    tone: a.signInWarn ? 'warn' : 'ok',
    text: d == null ? 'Signed in' : `${days(d)} left`,
    sub: a.signInExpiresAt && `until ${fmtDate(a.signInExpiresAt)}`,
  };
}

function tokenStatus(a) {
  if (!a.hasToken) return { tone: 'off', text: 'None' };
  if (a.tokenWrongAccount) return { tone: 'bad', text: 'Wrong account', sub: 'Make it again' };
  const d = a.tokenDaysLeft;
  if (d != null && d <= 0) return { tone: 'bad', text: 'Expired', sub: a.tokenExpiresAt && `on ${fmtDate(a.tokenExpiresAt)}` };
  if (a.tokenWarn) return { tone: 'warn', text: `${days(d)} left`, sub: `until ${fmtDate(a.tokenExpiresAt)}` };
  return { tone: 'ok', text: `Until ${fmtDate(a.tokenExpiresAt)}`, sub: a.tokenUnchecked ? 'Account not checked' : undefined };
}

// ------------------------------------------------------------------ state

let accounts = [];
let running = [];
let loaded = false;
const find = (slot) => accounts.find((a) => a.slot === slot) || { slot };

async function load() {
  try {
    const s = await api('/api/state');
    accounts = (s.accounts || []).sort((x, y) => x.slot - y.slot);
    running = s.flows || [];
    loaded = true;
    render();
  } catch (e) {
    if (e.status === 401) return;
    if (loaded) setSummary('bad', e.message);
    else renderError(e.message);
  }
}

// ------------------------------------------------------------------ the list

const list = $('#list');

function setSummary(tone, text) {
  $('#summary').innerHTML = text ? `<span class="dot ${tone}"></span>${esc(text)}` : '';
}

function stat(label, s, cls = '') {
  const sub = s.sub ? `<dd class="sub" title="${esc(s.sub)}"><span>${esc(s.sub)}</span></dd>` : '';
  return `<div class="stat is-${s.tone} ${cls}"><dt>${label}</dt><dd><span class="dot ${s.tone}"></span><span>${esc(s.text)}</span></dd>${sub}</div>`;
}

function usageStat(a) {
  const u = a.usage;
  const win = (label, w) => {
    if (!w) return '';
    const pct = Math.max(0, Math.min(100, Math.round(w.pct)));
    const tone = pct >= 95 ? 'is-bad' : pct >= 80 ? 'is-warn' : '';
    const resets = w.resetsAt ? ` title="Resets ${esc(new Date(w.resetsAt).toLocaleString())}"` : '';
    return `<span class="meter ${tone}"${resets}><span>${label}</span><span class="bar"><i data-pct="${pct}"></i></span><span class="pct">${pct}%</span></span>`;
  };
  const body = u && (u.fiveHour || u.sevenDay) ? win('5h', u.fiveHour) + win('7d', u.sevenDay) : '';
  return `<div class="stat stat-usage ${body ? '' : 'is-empty'}"><dt>Usage</dt>${body ? `<dd class="meters">${body}</dd>` : '<dd class="sub">—</dd>'}</div>`;
}

function nextAction(a, s, t) {
  const fl = running.find((f) => f.slot === a.slot);
  if (fl) return { label: 'Continue', act: 'resume', strong: true };
  if (!a.signedInAt && !a.hasToken) return { label: 'Set up', act: 'setup', strong: true };
  if (s.tone !== 'ok') return { label: 'Sign in', act: 'sign_in', strong: s.tone !== 'warn' };
  if (t.tone !== 'ok') return { label: 'Make token', act: 'setup_token', strong: t.tone !== 'warn' };
  return null;
}

function row(a) {
  const s = signInStatus(a);
  const t = tokenStatus(a);
  const next = nextAction(a, s, t);
  const tier = tierLabel(a.tier);
  const isNew = !a.email && !a.signedInAt && !a.hasToken;
  const meta = isNew ? 'Not set up yet' : tier ? `<span class="tier">${tier}</span>` : '';
  const btn = next
    ? `<button class="btn btn-sm ${next.strong ? 'btn-primary' : ''}" type="button" data-act="${next.act}" data-slot="${a.slot}">${next.label}</button>`
    : '';
  return `<article class="acct" data-slot="${a.slot}">
    <div class="acct-id"><span class="num" aria-label="Account ${a.slot}">${a.slot}</span>
      <div class="who"><div class="email ${a.email ? '' : 'is-empty'}" title="${esc(a.email || '')}">${esc(a.email || `Account ${a.slot}`)}</div>${meta ? `<div class="meta">${meta}</div>` : ''}</div></div>
    <dl class="acct-stats">${stat('Sign-in', s)}${stat('Setup token', t)}${usageStat(a)}</dl>
    <div class="acct-actions">${btn}<button class="icon-btn" type="button" data-act="menu" data-slot="${a.slot}" aria-haspopup="menu" aria-expanded="false" aria-label="More for account ${a.slot}">${icon('more')}</button></div>
  </article>`;
}

function render() {
  list.setAttribute('aria-busy', 'false');
  $('#add').hidden = accounts.length === 0;
  if (accounts.length === 0) {
    setSummary('', '');
    list.innerHTML = `<div class="empty">
      <div class="empty-icon">${icon('users')}</div>
      <h2>No accounts yet</h2>
      <p>Add each Claude account your machines should use.</p>
      <button class="btn btn-primary" type="button" data-act="add">Add account</button></div>`;
    return;
  }
  list.innerHTML = accounts.map(row).join('');
  for (const el of list.querySelectorAll('[data-pct]')) el.style.setProperty('--pct', `${el.dataset.pct}%`);
  const attention = accounts.filter((a) => signInStatus(a).tone !== 'ok' || tokenStatus(a).tone !== 'ok');
  const bad = attention.some((a) => ['bad', 'off'].includes(signInStatus(a).tone) || ['bad', 'off'].includes(tokenStatus(a).tone));
  if (attention.length === 0) setSummary('ok', accounts.length === 1 ? 'Ready' : `All ${accounts.length} ready`);
  else setSummary(bad ? 'bad' : 'warn', `${attention.length} of ${accounts.length} need${attention.length === 1 ? 's' : ''} attention`);
}

function renderError(message) {
  list.setAttribute('aria-busy', 'false');
  setSummary('', '');
  list.innerHTML = `<div class="empty">
    <div class="empty-icon is-bad">${icon('alert')}</div>
    <h2>${esc(message)}</h2>
    <p>Check that the hub is running, then try again.</p>
    <button class="btn" type="button" data-act="reload">Try again</button></div>`;
}

// ------------------------------------------------------------------ toast

let toastTimer;
function toast(message) {
  const t = $('#toast');
  t.innerHTML = `${icon('alert')}<span>${esc(message)}</span>`;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 6000);
}

// ------------------------------------------------------------------ the account menu

const menu = $('#menu');
let menuButton = null;

function openMenu(btn, slot) {
  const a = find(slot);
  menu.innerHTML = `
    <button type="button" role="menuitem" data-act="sign_in" data-slot="${slot}">${icon('login')}${a.signedInAt ? 'Sign in again' : 'Sign in'}</button>
    <button type="button" role="menuitem" data-act="setup_token" data-slot="${slot}">${icon('key')}${a.hasToken ? 'New setup token' : 'Make setup token'}</button>
    <hr>
    <button type="button" role="menuitem" class="is-danger" data-act="delete" data-slot="${slot}">${icon('trash')}Delete account…</button>`;
  menu.showPopover();
  const r = btn.getBoundingClientRect();
  const w = menu.offsetWidth;
  const h = menu.offsetHeight;
  const left = Math.max(8, Math.min(r.right - w, innerWidth - w - 8));
  const top = r.bottom + 4 + h > innerHeight - 8 ? Math.max(8, r.top - h - 4) : r.bottom + 4;
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  menuButton = btn;
  btn.setAttribute('aria-expanded', 'true');
  menu.querySelector('button').focus();
}

menu.addEventListener('toggle', (e) => {
  if (e.newState === 'closed' && menuButton) {
    menuButton.setAttribute('aria-expanded', 'false');
    if (menu.contains(document.activeElement) || document.activeElement === document.body) menuButton.focus();
    menuButton = null;
  }
});
menu.addEventListener('keydown', (e) => {
  const items = [...menu.querySelectorAll('button')];
  const i = items.indexOf(document.activeElement);
  if (e.key === 'ArrowDown') { e.preventDefault(); items[(i + 1) % items.length].focus(); }
  if (e.key === 'ArrowUp') { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
  if (e.key === 'Home') { e.preventDefault(); items[0].focus(); }
  if (e.key === 'End') { e.preventDefault(); items[items.length - 1].focus(); }
});
addEventListener('scroll', () => { if (menu.matches(':popover-open')) menu.hidePopover(); }, { passive: true });
addEventListener('resize', () => { if (menu.matches(':popover-open')) menu.hidePopover(); });

// ------------------------------------------------------------------ actions

async function addAccount(btn) {
  btn?.classList.add('is-loading');
  try {
    const r = await api('/api/accounts', { method: 'POST' });
    await load();
    openFlow({ slot: r.slot, mode: 'setup' });
  } catch (e) {
    if (e.status !== 401) toast(`Couldn't add an account: ${e.message}`);
  } finally {
    btn?.classList.remove('is-loading');
  }
}

const confirmDlg = $('#confirm');
let confirmSlot = null;

function askDelete(slot) {
  const a = find(slot);
  confirmSlot = slot;
  $('#confirm-title').textContent = `Delete account ${slot}?`;
  $('#confirm-sub').textContent = a.email || '';
  $('#confirm-text').textContent = `The hub signs it out, and every machine clears account ${slot}.`;
  confirmDlg.showModal();
  $('#confirm-no').focus();
}

$('#confirm-no').addEventListener('click', () => confirmDlg.close());
$('#confirm-yes').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.classList.add('is-loading');
  try {
    await api(`/api/accounts/${confirmSlot}`, { method: 'DELETE' });
    confirmDlg.close();
    await load();
  } catch (err) {
    if (err.status !== 401) toast(`Couldn't delete account ${confirmSlot}: ${err.message}`);
    confirmDlg.close();
  } finally {
    btn.classList.remove('is-loading');
  }
});

document.addEventListener('click', (e) => {
  const el = e.target.closest('[data-act]');
  if (!el || el.closest('#flow')) return;
  const slot = Number(el.dataset.slot);
  const act = el.dataset.act;
  if (menu.contains(el)) menu.hidePopover();
  switch (act) {
    case 'menu': return openMenu(el, slot);
    case 'add': return addAccount(el);
    case 'reload': list.setAttribute('aria-busy', 'true'); return load();
    case 'setup': case 'sign_in': case 'setup_token': return openFlow({ slot, mode: act });
    case 'resume': {
      const fl = running.find((f) => f.slot === slot);
      return fl ? openFlow({ slot, mode: fl.kind, resume: fl.id }) : load();
    }
    case 'delete': return askDelete(slot);
    default: return undefined;
  }
});
$('#add').addEventListener('click', (e) => addAccount(e.currentTarget));

// ------------------------------------------------------------------ flows

const flowDlg = $('#flow');
let run = null;

const RUNNING = ['starting', 'link', 'checking'];
const PART_LABEL = { sign_in: 'Sign in', setup_token: 'Setup token' };

function openFlow({ slot, mode, resume }) {
  closeFlow(false);
  run = {
    slot,
    parts: mode === 'setup' ? ['sign_in', 'setup_token'] : [mode],
    part: 0,
    id: resume || null,
    opened: false,
    timer: null,
    stage: 'starting',
    gen: 0,
  };
  buildView();
  flowDlg.showModal();
  if (resume) poll(run.gen);
  else startPart();
}

function closeFlow(refresh = true) {
  if (!run) return;
  clearTimeout(run.timer);
  run.gen += 1;
  run = null;
  if (flowDlg.open) flowDlg.close();
  if (refresh) load();
}

async function cancelFlow() {
  if (!run) return;
  const { id, stage } = run;
  closeFlow(false);
  if (id && RUNNING.includes(stage)) {
    try { await api(`/api/flows/${id}`, { method: 'DELETE' }); } catch { /* it ends by itself */ }
  }
  load();
}

flowDlg.addEventListener('cancel', (e) => { e.preventDefault(); cancelFlow(); });
flowDlg.addEventListener('click', (e) => {
  const el = e.target.closest('[data-f]');
  if (!el) return;
  if (el.dataset.f === 'cancel') cancelFlow();
  if (el.dataset.f === 'retry') startPart();
});

const kind = () => run.parts[run.part];

function buildView() {
  const a = find(run.slot);
  const k = kind();
  const title = run.parts.length > 1
    ? `Set up account ${run.slot}`
    : k === 'sign_in' ? `Sign in · Account ${run.slot}` : `New setup token · Account ${run.slot}`;
  const who = a.email ? `Sign in as ${esc(a.email)}.` : 'Sign in as the account you want here.';
  const hint = k === 'setup_token' && run.parts.length > 1
    ? 'Same account again: Claude asks once more, for the setup token.'
    : who;
  const parts = run.parts.length > 1
    ? `<ol class="parts">${run.parts.map((p, i) => `<li class="${i < run.part ? 'is-done' : i === run.part ? 'is-current' : ''}">${i + 1}. ${PART_LABEL[p]}</li>`).join('')}</ol>`
    : '';
  const checking = k === 'sign_in' ? 'Checking the code…' : 'Making the token and testing it…';
  flowDlg.innerHTML = `
    <div class="dlg-head">
      <div><h2 id="flow-title">${title}</h2>${a.email ? `<p class="dlg-sub">${esc(a.email)}</p>` : ''}</div>
      <button class="icon-btn" type="button" data-f="cancel" aria-label="Cancel">${icon('x')}</button>
    </div>
    ${parts}
    <div class="dlg-body" data-v="body">
      <ol class="steps">
        <li class="step" data-step="1"><span class="step-mark">1</span><div>
          <h3>Open Claude's sign-in page</h3>
          <div class="step-content keep"><p class="hint">${hint}</p><div data-v="link"></div></div>
        </div></li>
        <li class="step" data-step="2"><span class="step-mark">2</span><div>
          <h3>Paste the code</h3>
          <form class="step-content" data-v="form" novalidate>
            <p class="hint">Claude shows it after you approve.</p>
            <div data-v="note"></div>
            <div class="code-row">
              <label class="sr-only" for="code">Code from the sign-in page</label>
              <input class="input" id="code" name="code" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" placeholder="Paste the code" aria-describedby="code-error">
              <button class="btn btn-primary" type="submit" data-v="send">Continue</button>
            </div>
            <p class="field-error" id="code-error" data-v="err" hidden></p>
          </form>
        </div></li>
        <li class="step" data-step="3"><span class="step-mark">3</span><div>
          <h3>${k === 'sign_in' ? 'Finish signing in' : 'Save the token'}</h3>
          <div class="step-content"><div class="waiting"><span class="spinner"></span>${checking}</div></div>
        </div></li>
      </ol>
      <div data-v="fail"></div>
    </div>
    <div class="dlg-foot" data-v="foot">
      <button class="btn" type="button" data-f="cancel">Cancel</button>
      <button class="btn btn-primary" type="button" data-f="retry" hidden>Try again</button>
    </div>`;
  const form = $('[data-v="form"]', flowDlg);
  const input = $('#code', flowDlg);
  form.addEventListener('submit', (e) => { e.preventDefault(); sendCode(); });
  input.addEventListener('paste', () => setTimeout(() => { if (input.value.includes('#')) sendCode(); }, 0));
  input.addEventListener('input', () => setError(''));
}

function setError(msg) {
  const err = $('[data-v="err"]', flowDlg);
  const input = $('#code', flowDlg);
  if (!err) return;
  err.textContent = msg;
  err.hidden = !msg;
  if (msg) input.setAttribute('aria-invalid', 'true');
  else input.removeAttribute('aria-invalid');
}

function markStep(n, state) {
  const li = $(`[data-step="${n}"]`, flowDlg);
  li.classList.remove('is-current', 'is-done', 'is-failed');
  if (state) li.classList.add(`is-${state}`);
  $('.step-mark', li).innerHTML = state === 'done' ? icon('check') : state === 'failed' ? '!' : String(n);
}

async function startPart() {
  const r = run;
  r.gen += 1;
  const gen = r.gen;
  clearTimeout(r.timer);
  r.id = null;
  r.opened = false;
  buildView();
  update({ stage: 'starting' });
  try {
    const path = kind() === 'sign_in' ? 'sign-in' : 'setup-token';
    const res = await api(`/api/accounts/${r.slot}/${path}`, { method: 'POST' });
    if (run !== r || gen !== r.gen) return;
    r.id = res.id;
    poll(gen);
  } catch (e) {
    if (run === r && gen === r.gen) update({ stage: 'failed', message: e.message });
  }
}

async function poll(gen) {
  const r = run;
  if (!r || gen !== r.gen || !r.id) return;
  let f;
  try {
    f = await api(`/api/flows/${r.id}`);
  } catch (e) {
    if (run !== r || gen !== r.gen) return;
    if (e.status === 404) return update({ stage: 'failed', message: 'This sign-in is no longer running on the hub.' });
    if (e.status === 401) return undefined;
    r.timer = setTimeout(() => poll(gen), 3000);
    return undefined;
  }
  if (run !== r || gen !== r.gen) return undefined;
  update(f);
  if (RUNNING.includes(f.stage)) r.timer = setTimeout(() => poll(gen), f.stage === 'checking' ? 700 : 1200);
  return undefined;
}

function update(f) {
  const r = run;
  const prev = r.stage;
  r.stage = f.stage;
  const linkBox = $('[data-v="link"]', flowDlg);
  const note = $('[data-v="note"]', flowDlg);
  const fail = $('[data-v="fail"]', flowDlg);
  const retry = $('[data-f="retry"]', flowDlg);
  const input = $('#code', flowDlg);
  const send = $('[data-v="send"]', flowDlg);
  fail.innerHTML = '';
  retry.hidden = true;

  if (f.stage === 'starting') {
    markStep(1, 'current'); markStep(2, ''); markStep(3, '');
    linkBox.innerHTML = '<div class="waiting"><span class="spinner"></span>Getting the link…</div>';
    return;
  }

  if (f.stage === 'link') {
    if (!linkBox.querySelector('a') && f.url && f.url.startsWith('https://claude.ai/')) {
      linkBox.innerHTML = `<a class="btn btn-primary" data-v="open" href="${esc(f.url)}" target="_blank" rel="noopener noreferrer">Open sign-in page${icon('external')}</a>`;
      const a = $('[data-v="open"]', linkBox);
      const opened = () => {
        if (r.opened) return;
        r.opened = true;
        a.classList.remove('btn-primary');
        a.classList.add('btn-sm');
        a.innerHTML = `Open again${icon('external')}`;
        if (r.stage === 'link') { markStep(1, 'done'); markStep(2, 'current'); }
        setTimeout(() => input.focus(), 0);
      };
      for (const ev of ['click', 'auxclick', 'contextmenu']) a.addEventListener(ev, opened);
      if (!r.opened) a.focus();
    }
    input.disabled = false;
    send.classList.remove('is-loading');
    if (r.opened || f.message) {
      markStep(1, 'done'); markStep(2, 'current');
    } else {
      markStep(1, 'current'); markStep(2, '');
    }
    markStep(3, '');
    if (f.message) {
      note.innerHTML = `<div class="notice is-warn">${icon('alert')}<span class="notice-text">${esc(f.message)}</span></div>`;
      if (prev === 'checking') { input.value = ''; input.focus(); }
    } else {
      note.innerHTML = '';
    }
    return;
  }

  if (f.stage === 'checking') {
    markStep(1, 'done'); markStep(2, 'done'); markStep(3, 'current');
    note.innerHTML = '';
    return;
  }

  if (f.stage === 'done') return finishPart();

  // failed or cancelled
  const pasted = prev === 'checking' || $('[data-step="3"]', flowDlg).classList.contains('is-current');
  markStep(1, pasted ? 'done' : 'failed');
  markStep(2, pasted ? 'done' : '');
  markStep(3, pasted ? 'failed' : '');
  $('[data-step="1"] .step-content', flowDlg).classList.remove('keep');
  linkBox.innerHTML = '';
  input.disabled = true;
  const msg = f.stage === 'cancelled'
    ? 'This sign-in was stopped: another one started for this account.'
    : f.message || "It didn't finish.";
  fail.innerHTML = `<div class="notice is-bad">${icon('alert')}<span class="notice-text">${esc(msg)}</span></div>`;
  retry.hidden = false;
  retry.focus();
}

async function sendCode() {
  const r = run;
  if (!r || !r.id || r.stage !== 'link') return;
  const input = $('#code', flowDlg);
  const send = $('[data-v="send"]', flowDlg);
  const code = input.value.trim();
  if (!code) { setError('Paste the code from the sign-in page.'); return; }
  if (!code.includes('#')) { setError('That code is cut short: it has a # in the middle. Copy it again.'); return; }
  setError('');
  send.classList.add('is-loading');
  input.disabled = true;
  try {
    await api(`/api/flows/${r.id}/code`, { method: 'POST', body: { code } });
    if (run !== r) return;
    input.value = '';
    update({ stage: 'checking' });
  } catch (e) {
    if (run !== r) return;
    input.disabled = false;
    send.classList.remove('is-loading');
    setError(cap(e.message));
    input.focus();
  }
}

async function finishPart() {
  const r = run;
  const gen = r.gen;
  const k = kind();
  await load();
  if (run !== r || gen !== r.gen) return;
  const a = find(r.slot);
  const more = r.part < r.parts.length - 1;
  const title = k === 'sign_in' ? 'Signed in' : 'Setup token ready';
  const detail = k === 'sign_in'
    ? [a.email, a.signInExpiresAt && `until ${fmtDate(a.signInExpiresAt)}`].filter(Boolean).join(' · ')
    : [a.tokenExpiresAt && `Valid until ${fmtDate(a.tokenExpiresAt)}`, a.tokenUnchecked && "couldn't check which account it's for"].filter(Boolean).join(' · ');
  $('[data-v="body"]', flowDlg).innerHTML = `<div class="done" role="status">
    <div class="done-mark">${icon('check')}</div><h3>${title}</h3>${detail ? `<p>${esc(detail)}</p>` : ''}
    ${more ? `<p>Next: ${PART_LABEL[r.parts[r.part + 1]].toLowerCase()}</p>` : ''}</div>`;
  $('[data-v="foot"]', flowDlg).hidden = true;
  const parts = flowDlg.querySelectorAll('.parts li');
  if (parts[r.part]) { parts[r.part].className = 'is-done'; }
  r.timer = setTimeout(() => {
    if (run !== r || gen !== r.gen) return;
    if (more) { r.part += 1; startPart(); } else closeFlow(true);
  }, more ? 1100 : 1400);
}

// ------------------------------------------------------------------ start

load();
setInterval(() => { if (!document.hidden && !run) load(); }, 30000);
document.addEventListener('visibilitychange', () => { if (!document.hidden && !run) load(); });
