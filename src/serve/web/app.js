// chief dashboard (ARCHITECTURE §14). No framework and no build step: this file is served as it
// is. The board comes as server-sent events; everything else is a fetch.
(() => {
  'use strict';

  const $app = document.getElementById('app');
  const $toasts = document.getElementById('toasts');

  // ---- Small helpers --------------------------------------------------------------------

  /** h('div', {class: 'x', onclick: f}, child, 'text', [more]) */
  function h(tag, attrs, ...kids) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (k === 'html') el.innerHTML = v;
      else if (k === 'value') el.value = v;
      else if (k === 'checked') el.checked = v;
      else el.setAttribute(k, v === true ? '' : v);
    }
    for (const kid of kids.flat(Infinity)) {
      if (kid === null || kid === undefined || kid === false) continue;
      el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
    }
    return el;
  }

  /** Replaces el's children, with h()'s rules for them: nested arrays, and nothing for null. */
  function fill(el, ...kids) {
    el.replaceChildren(
      ...kids
        .flat(Infinity)
        .filter((kid) => kid !== null && kid !== undefined && kid !== false)
        .map((kid) => (kid.nodeType ? kid : document.createTextNode(String(kid)))),
    );
  }

  async function api(method, path, body) {
    const res = await fetch(path, {
      method,
      headers: body ? { 'content-type': 'application/json' } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    let data = null;
    try {
      data = await res.json();
    } catch {
      /* not JSON */
    }
    if (!res.ok) throw new Error((data && data.error) || `${res.status} ${res.statusText}`);
    return data;
  }

  function toast(text, bad) {
    const el = h('div', { class: 'toast' + (bad ? ' bad' : '') }, h('span', null, text));
    const close = () => el.remove();
    el.append(h('button', { 'aria-label': 'Dismiss', onclick: close }, '×'));
    $toasts.append(el);
    // A failure stays until it is read.
    if (!bad) setTimeout(close, 6000);
  }

  const esc = (s) =>
    String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

  function ago(ms) {
    if (!ms) return '';
    const s = Math.max(0, (Date.now() - ms) / 1000);
    if (s < 60) return 'now';
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    if (s < 86400) return `${Math.floor(s / 3600)}h`;
    return `${Math.floor(s / 86400)}d`;
  }

  function until(ms) {
    const s = (ms - Date.now()) / 1000;
    if (s < 45) return 'due';
    if (s < 3600) return `in ${Math.round(s / 60)}m`;
    if (s < 86400) return `in ${Math.round(s / 3600)}h`;
    return `in ${Math.round(s / 86400)}d`;
  }

  const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const stamp = (ms) =>
    new Date(ms).toLocaleString([], { weekday: 'short', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });

  // ---- Markdown, for reports: everything is escaped, only these tags are produced ---------

  function inline(text) {
    return text
      .split(/(`[^`\n]+`)/)
      .map((part) => {
        if (part.length > 2 && part.startsWith('`') && part.endsWith('`')) {
          return `<code>${esc(part.slice(1, -1))}</code>`;
        }
        return esc(part)
          .replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')
          .replace(/(^|[\s(])\*([^*\s][^*\n]*)\*(?=[\s).,;:!?]|$)/g, '$1<em>$2</em>')
          .replace(
            /\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g,
            '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>',
          )
          .replace(
            /(^|[\s(])(https?:\/\/[^\s<)]+[^\s<).,;:!?])/g,
            '$1<a href="$2" target="_blank" rel="noopener noreferrer">$2</a>',
          );
      })
      .join('');
  }

  function md(src) {
    const lines = String(src || '').replace(/\r\n?/g, '\n').split('\n');
    const out = [];
    let para = [];
    const flush = () => {
      if (para.length) out.push(`<p>${inline(para.join('\n')).replace(/\n/g, '<br>')}</p>`);
      para = [];
    };
    const cells = (line) =>
      line
        .trim()
        .replace(/^\||\|$/g, '')
        .split('|')
        .map((c) => inline(c.trim()));
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const fence = line.match(/^\s*```/);
      if (fence) {
        flush();
        const code = [];
        for (i++; i < lines.length && !/^\s*```/.test(lines[i]); i++) code.push(lines[i]);
        out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
        continue;
      }
      const heading = line.match(/^(#{1,4})\s+(.*)$/);
      if (heading) {
        flush();
        const level = Math.min(3, heading[1].length);
        out.push(`<h${level}>${inline(heading[2])}</h${level}>`);
        continue;
      }
      if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|?[\s:|-]+\|[\s:|-]*$/.test(lines[i + 1] || '')) {
        flush();
        const head = cells(line);
        const rows = [];
        for (i += 2; i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i]); i++) rows.push(cells(lines[i]));
        i--;
        out.push(
          `<table><thead><tr>${head.map((c) => `<th>${c}</th>`).join('')}</tr></thead><tbody>${rows
            .map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join('')}</tr>`)
            .join('')}</tbody></table>`,
        );
        continue;
      }
      const item = line.match(/^\s*([-*•]|\d+[.)])\s+(.*)$/);
      if (item) {
        flush();
        const ordered = /\d/.test(item[1]);
        const items = [];
        for (; i < lines.length; i++) {
          const m = lines[i].match(/^\s*([-*•]|\d+[.)])\s+(.*)$/);
          if (m) items.push(m[2]);
          else if (/^\s{2,}\S/.test(lines[i]) && items.length) items[items.length - 1] += '\n' + lines[i].trim();
          else break;
        }
        i--;
        const tag = ordered ? 'ol' : 'ul';
        out.push(`<${tag}>${items.map((t) => `<li>${inline(t).replace(/\n/g, '<br>')}</li>`).join('')}</${tag}>`);
        continue;
      }
      if (/^\s*>/.test(line)) {
        flush();
        const quote = [];
        for (; i < lines.length && /^\s*>/.test(lines[i]); i++) quote.push(lines[i].replace(/^\s*>\s?/, ''));
        i--;
        out.push(`<blockquote>${inline(quote.join('\n')).replace(/\n/g, '<br>')}</blockquote>`);
        continue;
      }
      if (!line.trim()) flush();
      else para.push(line);
    }
    flush();
    return out.join('');
  }

  // ---- Project list ---------------------------------------------------------------------

  async function projectList() {
    document.title = 'chief';
    let data;
    try {
      data = await api('GET', '/api/projects');
    } catch (e) {
      fill($app, h('p', { class: 'boot' }, e.message));
      return;
    }
    if (data.projects.length === 1 && !location.hash.includes('all')) {
      location.replace(`/p/${data.projects[0].slug}`);
      return;
    }
    const cards = data.projects.map((p) =>
      h(
        'a',
        { class: 'card', href: `/p/${p.slug}` },
        h('h2', null, p.name),
        p.goal && h('p', null, p.goal),
        h(
          'div',
          { class: 'sub' },
          p.waiting > 0 && h('span', { class: 'chip red' }, `${p.waiting} waiting on you`),
          p.working > 0 && h('span', { class: 'chip green' }, `${p.working} working`),
          h('span', { class: 'chip' }, p.open ? `${p.open} open thread${p.open === 1 ? '' : 's'}` : 'no open threads'),
          p.routines > 0 && h('span', { class: 'chip' }, `${p.routines} routine${p.routines === 1 ? '' : 's'}`),
        ),
      ),
    );
    fill($app, 
      h(
        'header',
        { class: 'topbar' },
        h('a', { class: 'brand', href: '/#all' }, h('img', { src: '/assets/icon.svg', width: 26, height: 26, alt: '' }), h('span', { class: 'project-name' }, 'chief')),
      ),
      h(
        'main',
        { class: 'projects' },
        h('h1', null, 'Projects'),
        h('p', null, 'The projects whose dashboard is on.'),
        cards.length
          ? cards
          : h(
              'div',
              { class: 'blank' },
              h('b', null, 'No project has its dashboard on'),
              'Turn one on from a terminal: ',
              h('code', null, 'chief dashboard <project>'),
            ),
      ),
    );
  }

  // ---- A project ------------------------------------------------------------------------

  const GLYPH = { waiting: '!', working: '●', limited: '◷', review: '◆', landing: '▲', idle: '○', failed: '✗', resolved: '✓' };

  function project(slug) {
    const base = `/api/p/${slug}`;
    const S = {
      board: null,
      sel: { kind: 'coordinator' },
      tab: 'conversation',
      view: 'board',
      showResolved: false,
      live: false,
      gone: null,
      detail: null,
      detailKey: '',
      projects: [],
    };
    let term = null; // the open terminal, if any
    let form = null; // the routine form's state, while one is open
    const $ = {};

    // -- Selection lives in the hash, so reload and back/forward keep it --

    function readHash() {
      const [, kind, name, tab] = location.hash.split('/').map(decodeURIComponent);
      if (kind === 't' && name) return { sel: { kind: 'thread', name }, tab: tab || null, picked: true };
      if (kind === 'r' && name) return { sel: { kind: 'routine', name }, tab: tab || null, picked: true };
      if (kind === 'o') return { sel: { kind: 'overview' }, tab: null, picked: true };
      if (kind === 'new-routine') return { sel: { kind: 'new-routine' }, tab: null, picked: true };
      if (kind === 'c') return { sel: { kind: 'coordinator' }, tab: name || null, picked: true };
      // With nothing picked: what the project follows, when it follows anything.
      const home = S.board && S.board.routines.length ? 'overview' : 'coordinator';
      return { sel: { kind: home }, tab: null, picked: false };
    }

    function hashFor(sel, tab) {
      if (sel.kind === 'thread') return `#/t/${encodeURIComponent(sel.name)}/${tab}`;
      if (sel.kind === 'routine') return `#/r/${encodeURIComponent(sel.name)}/${tab}`;
      if (sel.kind === 'new-routine') return '#/new-routine';
      if (sel.kind === 'overview') return '#/o';
      return `#/c/${tab}`;
    }

    function select(sel, tab) {
      const next = hashFor(sel, tab || defaultTab(sel));
      if (next === location.hash) {
        S.view = 'detail';
        apply();
      } else {
        location.hash = next;
      }
    }

    function showBoard() {
      S.view = 'board';
      $.body.dataset.view = 'board';
      // The terminal keeps its session attached only while it shows.
      closeTerm();
      history.replaceState(null, '', location.pathname);
      renderBoard();
    }

    const sameSel = (a, b) => a.kind === b.kind && a.name === b.name;
    const thread = (name) => S.board && S.board.threads.find((t) => t.name === name);
    const routine = (name) => S.board && S.board.routines.find((r) => r.name === name);
    const current = () => {
      if (!S.board) return null;
      if (S.sel.kind === 'coordinator') return S.board.coordinator;
      if (S.sel.kind === 'thread') return thread(S.sel.name);
      return null;
    };
    const attachable = (s) => !s || (!s.cleaned && !s.removed);

    function defaultTab(sel) {
      if (sel.kind === 'overview') return 'overview';
      if (sel.kind === 'new-routine') return 'settings';
      if (sel.kind === 'routine') {
        const r = routine(sel.name);
        return r && (r.last_run_at || r.last_finding) ? 'status' : 'settings';
      }
      if (sel.kind !== 'thread') return 'conversation';
      const t = thread(sel.name);
      return attachable(t) ? 'conversation' : 'reports';
    }

    // -- Shell --

    function shell() {
      $.name = h('span', { class: 'project-name' });
      $.goal = h('span', { class: 'goal' });
      $.live = h('span', { class: 'live down' }, h('i'), h('span', null, 'connecting'));
      $.board = h('nav', { class: 'board', 'aria-label': 'Board' });
      $.main = h('section', { class: 'main' });
      $.body = h('div', { class: 'body', 'data-view': S.view }, $.board, $.main);
      fill($app, 
        h(
          'header',
          { class: 'topbar' },
          h('a', { class: 'brand', href: '/#all', title: 'All projects' }, h('img', { src: '/assets/icon.svg', width: 26, height: 26, alt: 'chief' })),
          $.name,
          $.goal,
          $.live,
        ),
        $.body,
      );
    }

    function renderTop() {
      const b = S.board;
      if (S.projects.length > 1) {
        const pick = h(
          'select',
          { 'aria-label': 'Project', onchange: (e) => (location.href = `/p/${e.target.value}`) },
          S.projects.map((p) => h('option', { value: p.slug, selected: p.slug === slug }, p.name)),
        );
        fill($.name, pick);
      } else {
        $.name.textContent = b ? b.project.name : slug;
      }
      $.goal.textContent = (b && b.project.goal) || '';
      $.goal.title = $.goal.textContent;
      $.live.className = 'live' + (S.live ? '' : ' down');
      $.live.lastChild.textContent = S.live ? 'live' : 'reconnecting';
      const waiting = b ? b.threads.filter((t) => t.group === 'waiting').length : 0;
      document.title = `${waiting ? `(${waiting}) ` : ''}${b ? b.project.name : slug} · chief`;
    }

    // -- Board --

    function chips(t) {
      const out = [];
      if (t.waiting_reason) out.push(h('span', { class: 'chip red' }, t.waiting_reason));
      if (t.running) out.push(h('span', { class: 'chip green' }, t.running));
      if (t.limit) out.push(h('span', { class: 'chip amber' }, 'usage limit'));
      if (t.asleep) out.push(h('span', { class: 'chip' }, 'asleep'));
      if (t.pr) {
        const pr = t.pr;
        const [label, colour] =
          pr.state === 'MERGED' ? ['PR merged', 'purple']
          : pr.state === 'CLOSED' ? ['PR closed', '']
          : pr.draft ? ['PR draft', '']
          : pr.review === 'APPROVED' ? ['PR approved', 'green']
          : pr.review === 'CHANGES_REQUESTED' ? ['PR changes', 'amber']
          : ['PR open', 'blue'];
        out.push(h('span', { class: `chip ${colour}` }, label));
      }
      if (t.finding && t.finding.outcome === 'attention') out.push(h('span', { class: 'chip red', title: t.finding.headline }, 'needs you'));
      if (t.routine) out.push(h('span', { class: 'chip', title: `A run of the routine ${t.routine}` }, `↻ ${t.routine}`));
      if (t.cleaned) out.push(h('span', { class: 'chip' }, 'cleaned up'));
      else if (t.removed) out.push(h('span', { class: 'chip' }, 'removed'));
      else if (t.cleanup_blocked) out.push(h('span', { class: 'chip amber' }, 'cleanup blocked'));
      if (t.account) out.push(h('span', { class: 'chip', title: 'The Claude account it runs in' }, t.account));
      return out;
    }

    function sessionState(s) {
      if (!s) return ['not started', 'idle'];
      if (!s.started) return ['starting', 'idle'];
      if (s.asleep) return ['asleep', 'idle'];
      const label = S.board.groups.find((g) => g.key === s.group);
      let text = label ? label.label.toLowerCase() : s.group;
      if (s.group === 'waiting' && s.waiting_reason) text = `waiting on you · ${s.waiting_reason}`;
      if (s.group === 'resolved' && s.resolved_reason) text = `resolved · ${s.resolved_reason}`;
      return [text, s.group];
    }

    function threadRow(t) {
      const sel = { kind: 'thread', name: t.name };
      const detail = t.asks ? `${t.asks.label} ${t.asks.text}` : t.doing && t.doing.text ? t.doing.text : null;
      return h(
        'button',
        {
          class: 'row' + (t.group === 'resolved' ? ' resolved' : ''),
          'aria-current': S.view === 'detail' || wide() ? String(sameSel(S.sel, sel)) : 'false',
          onclick: () => select(sel),
        },
        h('span', { class: `glyph g-${t.group}`, 'aria-hidden': 'true' }, GLYPH[t.group] || '○'),
        h('span', { class: 'title' }, t.title || t.name),
        h('span', { class: 'when', title: stamp(t.last_activity_at) }, ago(t.last_activity_at)),
        h('span', { class: 'sub' }, chips(t), detail && h('span', { class: 'text' }, detail)),
      );
    }

    const wide = () => window.matchMedia('(min-width: 861px)').matches;

    function renderBoard() {
      const b = S.board;
      if (!b) return;
      const scroll = $.board.scrollTop;
      const kids = [];
      const waiting = b.threads.filter((t) => t.group === 'waiting');
      if (waiting.length) {
        kids.push(
          h(
            'button',
            { class: 'attention', onclick: () => select({ kind: 'thread', name: waiting[0].name }) },
            h('span', null, `${waiting.length} thread${waiting.length === 1 ? ' is' : 's are'} waiting on you`),
            h('small', null, 'Open →'),
          ),
        );
      }
      if (b.error) kids.push(h('div', { class: 'notice' }, `Showing the last known state: ${b.error}`));

      if (b.routines.length) {
        const oSel = { kind: 'overview' };
        const flagged = b.routines.filter((r) => r.last_finding && r.last_finding.outcome === 'attention').length;
        kids.push(
          h(
            'button',
            {
              class: 'row',
              'aria-current': S.view === 'detail' || wide() ? String(sameSel(S.sel, oSel)) : 'false',
              onclick: () => select(oSel),
            },
            h('span', { class: 'glyph g-idle', 'aria-hidden': 'true' }, '▦'),
            h('span', { class: 'title' }, 'Overview'),
            h('span', { class: 'when' }, ''),
            h('span', { class: 'sub' }, h('span', { class: 'text' }, `${b.routines.length} routine${b.routines.length === 1 ? '' : 's'}`), flagged > 0 && h('span', { class: 'chip red' }, `${flagged} need${flagged === 1 ? 's' : ''} you`)),
          ),
        );
      }

      const [cState, cGroup] = sessionState(b.coordinator);
      const cSel = { kind: 'coordinator' };
      kids.push(
        h(
          'button',
          {
            class: 'row',
            'aria-current': S.view === 'detail' || wide() ? String(sameSel(S.sel, cSel)) : 'false',
            onclick: () => select(cSel),
          },
          h('span', { class: `glyph g-${cGroup}`, 'aria-hidden': 'true' }, '◉'),
          h('span', { class: 'title' }, 'Coordinator'),
          h('span', { class: 'when' }, b.coordinator ? ago(b.coordinator.last_activity_at) : ''),
          h(
            'span',
            { class: 'sub' },
            h('span', { class: 'text' }, b.coordinator ? cState : 'Open it to start the conversation'),
            b.guard_denials > 0 && h('span', { class: 'chip amber', title: 'Messages the guard blocked' }, `${b.guard_denials} guard denial${b.guard_denials === 1 ? '' : 's'}`),
          ),
        ),
      );

      const open = b.threads.filter((t) => t.group !== 'resolved');
      if (!b.threads.length) {
        kids.push(h('div', { class: 'section-head' }, 'Threads'), h('div', { class: 'empty' }, 'No threads yet. Tell the coordinator what you want done.'));
      }
      for (const g of b.groups) {
        const rows = b.threads.filter((t) => t.group === g.key);
        if (!rows.length) continue;
        if (g.key === 'resolved') {
          kids.push(
            h(
              'div',
              { class: 'section-head' },
              g.label,
              h('span', { class: 'count' }, rows.length),
              h('span', { class: 'grow' }),
              h('button', { onclick: () => ((S.showResolved = !S.showResolved), renderBoard()) }, S.showResolved ? 'Hide' : 'Show'),
            ),
          );
          if (S.showResolved) kids.push(rows.map(threadRow));
        } else {
          kids.push(h('div', { class: `section-head ${g.key}` }, g.label, h('span', { class: 'count' }, rows.length)), rows.map(threadRow));
        }
      }
      if (b.threads.length && !open.length && !S.showResolved) {
        kids.push(h('div', { class: 'empty' }, 'Every thread is resolved.'));
      }

      kids.push(
        h(
          'div',
          { class: 'section-head' },
          'Routines',
          h('span', { class: 'count' }, b.routines.length || ''),
          h('span', { class: 'grow' }),
          h('button', { onclick: () => select({ kind: 'new-routine' }) }, '+ New'),
        ),
      );
      if (!b.routines.length) {
        kids.push(h('div', { class: 'empty' }, 'Work that repeats on a schedule. Ask the coordinator for one, or add it here.'));
      }
      for (const r of b.routines) {
        const sel = { kind: 'routine', name: r.name };
        kids.push(
          h(
            'button',
            {
              class: 'row' + (r.enabled ? '' : ' resolved'),
              'aria-current': S.view === 'detail' || wide() ? String(sameSel(S.sel, sel)) : 'false',
              onclick: () => select(sel),
            },
            h(
              'span',
              { class: 'glyph ' + (r.last_finding && r.last_finding.outcome === 'attention' ? 'g-waiting' : 'g-idle'), 'aria-hidden': 'true' },
              !r.enabled ? '‖' : r.last_finding && r.last_finding.outcome === 'attention' ? '!' : '↻',
            ),
            h('span', { class: 'title' }, r.title),
            h(
              'span',
              { class: 'when', title: r.next_run_at ? stamp(r.next_run_at) : '' },
              !r.enabled ? 'paused' : r.next_run_at ? until(r.next_run_at) : 'never due',
            ),
            h(
              'span',
              { class: 'sub' },
              h('span', { class: 'text' }, r.last_finding ? r.last_finding.headline : r.schedule_text),
              r.last_note && h('span', { class: 'chip amber', title: r.last_note }, 'see note'),
            ),
          ),
        );
      }
      fill($.board, kids);
      $.board.scrollTop = scroll;
    }

    // -- Main: head --

    /** A button that asks once more before it acts: the second click within 4 s runs it. */
    function confirmButton(label, sure, run, cls) {
      const btn = h('button', { class: `btn ${cls || ''}` }, label);
      let armed = null;
      btn.addEventListener('click', () => {
        if (armed) {
          clearTimeout(armed);
          armed = null;
          btn.textContent = label;
          btn.classList.remove('armed');
          run(btn);
          return;
        }
        btn.textContent = sure;
        btn.classList.add('armed');
        armed = setTimeout(() => {
          armed = null;
          btn.textContent = label;
          btn.classList.remove('armed');
        }, 4000);
      });
      return btn;
    }

    async function act(name, action, btn) {
      if (btn) btn.disabled = true;
      try {
        const done = await api('POST', `${base}/threads/${encodeURIComponent(name)}/${action}`);
        toast(done.message);
      } catch (e) {
        toast(e.message, true);
      } finally {
        if (btn) btn.disabled = false;
      }
    }

    function threadActions(t) {
      const out = [];
      if (t.pr) out.push(h('a', { class: 'btn', href: t.pr.url, target: '_blank', rel: 'noopener noreferrer' }, 'Open PR ↗'));
      const gone = t.cleaned || t.removed;
      if (!gone && t.group !== 'resolved') {
        if (t.asleep) out.push(h('button', { class: 'btn', onclick: (e) => act(t.name, 'wake', e.currentTarget) }, 'Wake'));
        else if (t.started) out.push(confirmButton('Stop', 'Stop it?', (btn) => act(t.name, 'stop', btn), 'danger'));
        out.push(h('button', { class: 'btn', onclick: (e) => act(t.name, 'resolve', e.currentTarget) }, 'Resolve'));
      }
      if (t.group === 'resolved' && !gone) {
        out.push(h('button', { class: 'btn', onclick: (e) => act(t.name, 'reopen', e.currentTarget) }, 'Reopen'));
        out.push(confirmButton('Clean up', 'Remove worktree?', (btn) => act(t.name, 'clean', btn), ''));
      }
      return out;
    }

    function callouts(s) {
      const out = [];
      if (!s) return out;
      if (s.asks) {
        out.push(
          h(
            'div',
            { class: 'callout red' },
            h('div', { class: 'what' }, h('b', null, 'Waiting on you'), `It ${s.asks.label} `, h('code', null, s.asks.text)),
            S.tab !== 'conversation' && attachable(s) && h('button', { class: 'btn', onclick: () => select(S.sel, 'conversation') }, 'Answer it'),
          ),
        );
      } else if (s.group === 'waiting') {
        out.push(
          h(
            'div',
            { class: 'callout red' },
            h('div', { class: 'what' }, h('b', null, 'Waiting on you'), s.waiting_reason === 'permission' ? 'It asks for permission in its conversation.' : 'It asked a question in its conversation.'),
            S.tab !== 'conversation' && attachable(s) && h('button', { class: 'btn', onclick: () => select(S.sel, 'conversation') }, 'Answer it'),
          ),
        );
      }
      if (s.limit) out.push(h('div', { class: 'callout amber' }, h('div', { class: 'what' }, h('b', null, 'Stopped on a usage limit'), s.limit)));
      if (s.cleanup_blocked && !s.cleaned) out.push(h('div', { class: 'callout amber' }, h('div', { class: 'what' }, h('b', null, 'Cleanup blocked'), s.cleanup_blocked)));
      if (s.removed) out.push(h('div', { class: 'callout' }, h('div', { class: 'what' }, h('b', null, 'Removed outside chief'), 'Its session is gone; its reports are still here.')));
      else if (s.cleaned) out.push(h('div', { class: 'callout' }, h('div', { class: 'what' }, h('b', null, 'Cleaned up'), 'Its session, worktree and local branch were removed; its reports are still here.')));
      return out;
    }

    function tabsFor(s) {
      const tabs = [];
      if (attachable(s)) tabs.push(['conversation', 'Conversation']);
      tabs.push(['reports', 'Reports']);
      if (S.sel.kind === 'thread') tabs.push(['activity', 'Activity']);
      else tabs.push(['project', 'Project']);
      return tabs;
    }

    function renderHead() {
      const b = S.board;
      const back = h('button', { class: 'back', 'aria-label': 'Back to the board', onclick: showBoard }, '‹');
      if (S.sel.kind === 'overview') {
        fill(
          $.head,
          h('div', { class: 'head-line' }, back, h('h1', null, 'Overview')),
          h('div', { class: 'meta' }, h('span', null, 'What this project follows, and what needs you.')),
          h('div', { style: 'height:12px' }),
        );
        fill($.callouts);
        return;
      }
      if (S.sel.kind === 'routine' || S.sel.kind === 'new-routine') {
        const r = S.sel.kind === 'routine' ? routine(S.sel.name) : null;
        fill($.head, 
          h(
            'div',
            { class: 'head-line' },
            back,
            h('h1', null, r ? r.title : S.sel.kind === 'new-routine' ? 'New routine' : S.sel.name),
            r && h('span', { class: 'state ' + (r.enabled ? 'g-working' : 'g-idle') }, r.enabled ? 'on' : 'paused'),
          ),
          h(
            'div',
            { class: 'meta' },
            r
              ? [
                  h('span', null, r.schedule_text),
                  r.enabled && r.next_run_at && h('span', null, 'next ', h('b', { title: stamp(r.next_run_at) }, `${stamp(r.next_run_at)} (${until(r.next_run_at)})`)),
                  r.last_run_at && h('span', null, 'last run ', h('b', null, stamp(r.last_run_at))),
                ]
              : h('span', null, 'A prompt chief runs on a schedule. Each run is a fresh thread.'),
          ),
          r
            ? h(
                'div',
                { class: 'tabs', role: 'tablist' },
                [['status', 'Status'], ['settings', 'Settings']].map(([key, label]) =>
                  h('button', { class: 'tab', role: 'tab', 'aria-selected': String(S.tab === key), onclick: () => select(S.sel, key) }, label),
                ),
              )
            : h('div', { style: 'height:12px' }),
        );
        fill($.callouts, 
          ...(r && r.last_note ? [h('div', { class: 'callout amber' }, h('div', { class: 'what' }, h('b', null, 'Last occurrence'), r.last_note))] : []),
        );
        return;
      }
      const s = current();
      const isCoord = S.sel.kind === 'coordinator';
      const [state, group] = b ? sessionState(s) : ['', 'idle'];
      const title = isCoord ? 'Coordinator' : s ? s.title || s.name : S.sel.name;
      const meta = [];
      if (isCoord) {
        meta.push(h('span', null, h('code', null, b ? b.coordinator_name : '')));
        if (s && s.model) meta.push(h('span', null, `${s.model} · ${s.effort || ''}`));
        if (s) meta.push(h('span', null, `active ${ago(s.last_activity_at)} ago`.replace('now ago', 'just now')));
        if (!s) meta.push(h('span', null, 'Not started yet: open the conversation to start it.'));
      } else if (s) {
        meta.push(h('span', null, h('code', null, s.name)));
        if (s.repo) meta.push(h('span', null, 'repo ', h('b', null, s.repo)));
        if (s.branch) meta.push(h('span', null, 'branch ', h('code', null, s.branch)));
        if (s.model) meta.push(h('span', null, `${s.model} · ${s.effort || ''}`));
        if (s.account) meta.push(h('span', null, 'account ', h('b', null, s.account)));
        if (s.routine) meta.push(h('span', null, 'run of ', h('a', { href: hashFor({ kind: 'routine', name: s.routine }) }, s.routine)));
        if (s.moved) meta.push(h('span', null, s.moved));
        meta.push(h('span', null, `active ${ago(s.last_activity_at)} ago`.replace('now ago', 'just now')));
      } else if (b) {
        meta.push(h('span', null, 'This thread is not on the board any more.'));
      }
      const tabs = tabsFor(s);
      fill($.head, 
        h(
          'div',
          { class: 'head-line' },
          back,
          h('h1', { title }, title),
          h('span', { class: `state g-${group}` }, state),
          h('span', { class: 'grow' }),
          h('div', { class: 'actions' }, !isCoord && s ? threadActions(s) : null),
        ),
        h('div', { class: 'meta' }, meta),
        h(
          'div',
          { class: 'tabs', role: 'tablist' },
          tabs.map(([key, label]) =>
            h('button', { class: 'tab', role: 'tab', 'aria-selected': String(S.tab === key), onclick: () => select(S.sel, key) }, label),
          ),
        ),
      );
      fill($.callouts, ...callouts(s));
    }

    // -- Main: the conversation --

    function closeTerm() {
      if (!term) return;
      term.closed = true;
      if (term.ws) term.ws.close();
      if (term.observer) term.observer.disconnect();
      if (term.x) term.x.dispose();
      term = null;
    }

    function openTerm(pane) {
      closeTerm();
      const target = S.sel.kind === 'thread' ? S.sel.name : null;
      const holder = h('div', { class: 'term' });
      const overlay = h('div', { class: 'overlay' });
      const screen = h('div', { class: 'screen' }, holder, overlay);
      fill(pane, 
        screen,
        h(
          'div',
          { class: 'term-bar' },
          h('span', null, 'This is the session itself, as ', h('code', null, 'claude attach'), ' shows it.'),
          h('span', { class: 'grow' }),
          h('span', { class: 'keys' }, 'Leaving this view detaches; the session keeps running.'),
        ),
      );
      const say = (text, kind) => {
        overlay.hidden = false;
        fill(overlay, 
          kind === 'busy' ? h('div', { class: 'spinner' }) : null,
          h('p', { class: kind === 'error' ? 'err' : '' }, text),
          kind === 'busy' ? null : h('button', { class: 'btn', onclick: () => openTerm(pane) }, kind === 'error' ? 'Try again' : 'Reconnect'),
        );
      };
      say(target ? 'Opening the thread…' : 'Opening the coordinator…', 'busy');

      const css = getComputedStyle(document.documentElement);
      const x = new Terminal({
        fontFamily: css.getPropertyValue('--mono'),
        fontSize: 13,
        lineHeight: 1.15,
        cursorBlink: true,
        scrollback: 8000,
        macOptionIsMeta: true,
        theme: { background: css.getPropertyValue('--screen').trim() || '#14161b', foreground: '#dfe1e6', cursor: '#5eead4' },
      });
      const fit = new FitAddon.FitAddon();
      x.loadAddon(fit);
      x.open(holder);
      const me = { x, ws: null, observer: null, closed: false };
      term = me;
      try {
        fit.fit();
      } catch {
        /* not laid out yet */
      }

      const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
      const query = new URLSearchParams({ cols: String(x.cols), rows: String(x.rows) });
      if (target) query.set('target', target);
      const ws = new WebSocket(`${scheme}://${location.host}${base}/term?${query}`);
      ws.binaryType = 'arraybuffer';
      me.ws = ws;
      const encoder = new TextEncoder();
      const send = (data) => ws.readyState === WebSocket.OPEN && ws.send(data);
      x.onData((data) => send(encoder.encode(data)));
      x.onBinary((data) => send(Uint8Array.from(data, (c) => c.charCodeAt(0))));
      x.onResize(({ cols, rows }) => send(JSON.stringify({ type: 'resize', cols, rows })));
      let ready = false;
      let ended = false;
      ws.onmessage = (event) => {
        if (typeof event.data !== 'string') {
          x.write(new Uint8Array(event.data));
          return;
        }
        let msg;
        try {
          msg = JSON.parse(event.data);
        } catch {
          return;
        }
        if (msg.type === 'status') say(msg.text, 'busy');
        else if (msg.type === 'ready') {
          ready = true;
          overlay.hidden = true;
          x.focus();
        } else if (msg.type === 'error') {
          ended = true;
          say(msg.text, 'error');
        } else if (msg.type === 'exit') {
          ended = true;
          say('Detached. The session keeps running.', 'done');
        }
      };
      ws.onclose = () => {
        if (me.closed || ended) return;
        say(ready ? 'The connection closed. The session keeps running.' : 'Could not reach chief serve.', ready ? 'done' : 'error');
      };
      me.observer = new ResizeObserver(() => {
        try {
          fit.fit();
        } catch {
          /* hidden */
        }
      });
      me.observer.observe(holder);
    }

    // -- Main: reports, activity, project files --

    async function loadDetail(force) {
      const s = current();
      const name = S.sel.kind === 'coordinator' ? S.board && S.board.coordinator_name : S.sel.name;
      if (!name || (S.sel.kind === 'coordinator' && !s)) {
        S.detail = null;
        return;
      }
      // Read again only when something it shows changed.
      const key = `${name}|${s ? s.last_report_at : ''}|${S.tab === 'activity' && s ? s.last_activity_at : ''}`;
      if (!force && key === S.detailKey && S.detail) return;
      S.detailKey = key;
      try {
        const detail = await api('GET', `${base}/sessions/${encodeURIComponent(name)}`);
        if (S.detailKey === key) S.detail = detail;
      } catch (e) {
        if (S.detailKey === key) S.detail = { error: e.message };
      }
    }

    function reportsPane() {
      const d = S.detail;
      if (!d) return h('div', { class: 'blank' }, S.sel.kind === 'coordinator' && !current() ? 'The coordinator has not started yet.' : 'Loading…');
      if (d.error) return h('div', { class: 'blank' }, d.error);
      const kids = [];
      if (d.task) kids.push(h('details', { class: 'task' }, h('summary', null, 'Its task'), h('div', { class: 'md', html: md(d.task) })));
      if (!d.reports.length) {
        kids.push(h('div', { class: 'blank' }, h('b', null, 'No report yet'), 'A session reports when it ends a turn.'));
      }
      d.reports.forEach((r, i) => {
        kids.push(
          h(
            'article',
            { class: 'report' },
            h('div', { class: 'report-head' }, h('b', null, i === 0 ? 'Latest report' : `Report ${d.reports.length - i}`), h('span', null, `${stamp(r.at)} · ${ago(r.at) === 'now' ? 'just now' : ago(r.at) + ' ago'}`)),
            h('div', { class: 'md', html: md(r.text) }),
          ),
        );
      });
      return h('div', { class: 'inner' }, kids);
    }

    function activityPane() {
      const d = S.detail;
      if (!d) return h('div', { class: 'blank' }, 'Loading…');
      if (d.error) return h('div', { class: 'blank' }, d.error);
      const kids = [];
      if (d.subagents.length) {
        kids.push(
          h(
            'div',
            { class: 'subagents' },
            h('h3', null, 'Subagents working'),
            d.subagents.map((a) => h('div', null, h('b', null, a.name || a.description || a.id), h('code', null, a.call ? `${a.call} · ${ago(a.since)}` : 'starting'))),
          ),
        );
      }
      if (!d.events.length) kids.push(h('div', { class: 'blank' }, h('b', null, 'Nothing recorded yet'), 'Its tool calls and turns show here as they happen.'));
      else
        kids.push(
          h(
            'ul',
            { class: 'events' },
            d.events.map((e) =>
              h('li', null, h('time', { title: stamp(e.at) }, clock(e.at)), h('span', { class: 'hook' + (e.agent ? ' sub-agent' : '') }, e.agent ? `${e.hook} · subagent` : e.hook), h('span', { class: 'sum' }, e.summary || '')),
            ),
          ),
        );
      return h('div', { class: 'inner' }, kids);
    }

    async function projectPane(pane) {
      fill(pane, h('div', { class: 'blank' }, 'Loading…'));
      let files;
      try {
        files = await api('GET', `${base}/files`);
      } catch (e) {
        fill(pane, h('div', { class: 'blank' }, e.message));
        return;
      }
      if (S.tab !== 'project' || S.sel.kind !== 'coordinator') return;
      const b = S.board.project;
      const strip = (text) => String(text || '').replace(/<!--[\s\S]*?-->/g, '').trim();
      const section = (title, hint, text) =>
        h('article', { class: 'report' }, h('div', { class: 'report-head' }, h('b', null, title), h('span', null, hint)), strip(text) ? h('div', { class: 'md', html: md(strip(text)) }) : h('p', { class: 'dim' }, 'Empty.'));
      fill(pane, 
        h(
          'div',
          { class: 'inner' },
          h(
            'article',
            { class: 'report' },
            h('div', { class: 'report-head' }, h('b', null, 'Settings'), h('span', null, 'project.toml · edit them on the terminal board (s)')),
            h(
              'div',
              { class: 'meta' },
              h('span', null, 'repositories ', h('b', null, b.repos.join(', ') || 'none')),
              h('span', null, 'threads ', h('b', null, `${b.thread_model} · ${b.thread_effort}`)),
              h('span', null, 'at most ', h('b', null, b.max_parallel), ' at once'),
            ),
          ),
          section('Instructions', 'instructions.md · sent to the coordinator and every new thread', files.instructions),
          section('Memory', 'memory/MEMORY.md · what the project remembers', files.memory),
        ),
      );
    }

    // -- Main: a routine --

    const DAYS = [
      [1, 'Mon'], [2, 'Tue'], [3, 'Wed'], [4, 'Thu'], [5, 'Fri'], [6, 'Sat'], [0, 'Sun'],
    ];

    /** A schedule as the builder's fields; anything it has no fields for is `custom`. */
    function parseSchedule(text) {
      const f = { mode: 'weekdays', time: '09:00', minute: 0, days: [1], n: 2, unit: 'h', custom: text || '' };
      const s = (text || '').trim().toLowerCase();
      if (!s) return f;
      const pad = (n) => String(n).padStart(2, '0');
      let m;
      if (s === '@hourly') return { ...f, mode: 'hourly', minute: 0 };
      if (s === '@daily' || s === '@midnight') return { ...f, mode: 'daily', time: '00:00' };
      if (s === '@weekdays') return { ...f, mode: 'weekdays', time: '09:00' };
      if ((m = s.match(/^every\s*(\d+)\s*(m|h|d)/))) return { ...f, mode: 'interval', n: Number(m[1]), unit: m[2] };
      if ((m = s.match(/^(\d{1,2}) \* \* \* \*$/))) return { ...f, mode: 'hourly', minute: Number(m[1]) };
      if ((m = s.match(/^(\d{1,2}) (\d{1,2}) \* \* (\*|1-5|[0-7](?:,[0-7])*)$/))) {
        const time = `${pad(m[2])}:${pad(m[1])}`;
        if (m[3] === '*') return { ...f, mode: 'daily', time };
        if (m[3] === '1-5') return { ...f, mode: 'weekdays', time };
        return { ...f, mode: 'weekly', time, days: m[3].split(',').map((d) => Number(d) % 7) };
      }
      return { ...f, mode: 'custom' };
    }

    function buildSchedule(f) {
      const [hh, mm] = (f.time || '09:00').split(':').map(Number);
      if (f.mode === 'hourly') return `${Number(f.minute) || 0} * * * *`;
      if (f.mode === 'daily') return `${mm} ${hh} * * *`;
      if (f.mode === 'weekdays') return `${mm} ${hh} * * 1-5`;
      if (f.mode === 'weekly') return `${mm} ${hh} * * ${[...f.days].sort((a, b) => a - b).join(',')}`;
      if (f.mode === 'interval') return `every ${Number(f.n) || 0}${f.unit}`;
      return f.custom.trim();
    }

    function routinePane(pane) {
      const existing = S.sel.kind === 'routine' ? routine(S.sel.name) : null;
      if (S.sel.kind === 'routine' && !existing) {
        fill(pane, h('div', { class: 'blank' }, h('b', null, 'No such routine'), 'It may have been deleted.'));
        return;
      }
      const p = S.board.project;
      const r = existing || { title: '', prompt: '', schedule: '0 9 * * 1-5', repo: null, model: null, effort: null, notify: true, enabled: true };
      form = { key: existing ? existing.name : '+new', sched: parseSchedule(r.schedule) };
      const f = form.sched;

      const title = h('input', { type: 'text', id: 'r-title', value: r.title, placeholder: 'Triage new issues', maxlength: 80 });
      const prompt = h('textarea', { id: 'r-prompt', placeholder: 'What every run does. A run sees neither the conversation nor earlier runs, and nobody is there to answer it: say what done means, what to report, and what to do when something is missing.' });
      prompt.value = r.prompt;

      const extra = h('div', { class: 'sched-row' });
      const preview = h('div', { class: 'hint' });
      const modes = [
        ['hourly', 'Hourly'], ['daily', 'Daily'], ['weekdays', 'Weekdays'], ['weekly', 'Weekly'], ['interval', 'Every…'], ['custom', 'Cron'],
      ];
      const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'How often' });
      const drawSchedule = () => {
        fill(seg, 
          ...modes.map(([key, label]) =>
            h('button', { type: 'button', 'aria-pressed': String(f.mode === key), onclick: () => ((f.mode = key), drawSchedule()) }, label),
          ),
        );
        const time = () => h('input', { type: 'time', value: f.time, 'aria-label': 'Time', onchange: (e) => ((f.time = e.target.value || '09:00'), drawPreview()) });
        const kids = [];
        if (f.mode === 'hourly') {
          kids.push('at minute', h('input', { type: 'number', min: 0, max: 59, value: f.minute, 'aria-label': 'Minute', onchange: (e) => ((f.minute = Math.min(59, Math.max(0, Number(e.target.value) || 0))), drawPreview()) }), 'of every hour');
        } else if (f.mode === 'daily' || f.mode === 'weekdays') {
          kids.push(f.mode === 'daily' ? 'every day at' : 'Monday to Friday at', time());
        } else if (f.mode === 'weekly') {
          kids.push(
            h(
              'div',
              { class: 'days', role: 'group', 'aria-label': 'Days' },
              DAYS.map(([n, label]) =>
                h(
                  'button',
                  {
                    type: 'button',
                    'aria-pressed': String(f.days.includes(n)),
                    onclick: () => {
                      f.days = f.days.includes(n) ? f.days.filter((d) => d !== n) : [...f.days, n];
                      if (!f.days.length) f.days = [n];
                      drawSchedule();
                    },
                  },
                  label,
                ),
              ),
            ),
            'at',
            time(),
          );
        } else if (f.mode === 'interval') {
          kids.push(
            'every',
            h('input', { type: 'number', min: 1, value: f.n, 'aria-label': 'How many', onchange: (e) => ((f.n = Math.max(1, Number(e.target.value) || 1)), drawPreview()) }),
            h(
              'select',
              { 'aria-label': 'Unit', onchange: (e) => ((f.unit = e.target.value), drawPreview()) },
              [['m', 'minutes'], ['h', 'hours'], ['d', 'days']].map(([v, label]) => h('option', { value: v, selected: f.unit === v }, label)),
            ),
            'after the last run',
          );
        } else {
          kids.push(h('input', { type: 'text', value: f.custom, placeholder: '0 9 * * 1-5', 'aria-label': 'Cron expression', style: 'width:220px;font-family:var(--mono)', oninput: (e) => ((f.custom = e.target.value), drawPreview()) }));
        }
        fill(extra, ...kids.map((k) => (typeof k === 'string' ? h('span', { class: 'dim' }, k) : k)));
        drawPreview();
      };
      const drawPreview = () => {
        const text = buildSchedule(f);
        fill(preview, 
          f.mode === 'custom' ? 'Five cron fields: minute, hour, day of month, month, day of week. ' : ['Saved as ', h('code', null, text), '. '],
          "Times are this machine's local time; a routine runs at most every 5 minutes.",
        );
      };
      drawSchedule();

      const repo = h(
        'select',
        { id: 'r-repo' },
        h('option', { value: '' }, p.default_repo ? `Default (${p.default_repo})` : 'Default'),
        p.repos.map((name) => h('option', { value: name, selected: r.repo === name }, name)),
      );
      const model = h('input', { type: 'text', id: 'r-model', value: r.model || '', placeholder: `Default (${p.thread_model})` });
      const effort = h(
        'select',
        { id: 'r-effort' },
        h('option', { value: '' }, `Default (${p.thread_effort})`),
        ['low', 'medium', 'high', 'xhigh', 'max'].map((e) => h('option', { value: e, selected: r.effort === e }, e)),
      );
      const notify = h('input', { type: 'checkbox', id: 'r-notify', checked: r.notify });
      const error = h('div', { class: 'form-error', hidden: true });

      const fail = (e) => {
        error.textContent = e.message;
        error.hidden = false;
      };
      const body = () => ({
        title: title.value,
        prompt: prompt.value,
        schedule: buildSchedule(f),
        repo: repo.value,
        model: model.value,
        effort: effort.value,
        notify: notify.checked,
      });
      const save = async (btn) => {
        error.hidden = true;
        btn.disabled = true;
        try {
          if (existing) {
            await api('PATCH', `${base}/routines/${encodeURIComponent(existing.name)}`, body());
            toast(`Saved ${existing.title}.`);
          } else {
            const made = await api('POST', `${base}/routines`, body());
            toast(`Added ${made.title} · ${made.schedule_text}.`);
            form = null;
            select({ kind: 'routine', name: made.name });
          }
        } catch (e) {
          fail(e);
        } finally {
          btn.disabled = false;
        }
      };
      const runNow = async (btn) => {
        btn.disabled = true;
        try {
          const ran = await api('POST', `${base}/routines/${encodeURIComponent(existing.name)}/run`);
          if (ran.started) {
            toast(`Started ${ran.thread}.`);
            select({ kind: 'thread', name: ran.thread });
          } else {
            toast(`Nothing started: ${ran.note}.`, true);
          }
        } catch (e) {
          toast(e.message, true);
        } finally {
          btn.disabled = false;
        }
      };
      const remove = async () => {
        try {
          await api('DELETE', `${base}/routines/${encodeURIComponent(existing.name)}`);
          toast(`Deleted ${existing.title}. Its past runs stay.`);
          form = null;
          select({ kind: 'coordinator' });
        } catch (e) {
          toast(e.message, true);
        }
      };

      // Pausing leaves the rest of the form, and whatever is typed in it, as it is.
      let enabled = existing ? existing.enabled : true;
      const toggle = h('button', { class: 'btn', type: 'button' }, enabled ? 'Pause' : 'Resume');
      toggle.addEventListener('click', async () => {
        toggle.disabled = true;
        try {
          await api('PATCH', `${base}/routines/${encodeURIComponent(existing.name)}`, { enabled: !enabled });
          enabled = !enabled;
          toggle.textContent = enabled ? 'Pause' : 'Resume';
          toast(enabled ? `Resumed ${existing.title}.` : `Paused ${existing.title}.`);
        } catch (e) {
          toast(e.message, true);
        } finally {
          toggle.disabled = false;
        }
      });

      const runs = h('div', { class: 'runs' });
      $.runs = runs;
      fill(pane, 
        h(
          'form',
          { class: 'form', onsubmit: (e) => e.preventDefault() },
          h('div', { class: 'field' }, h('label', { for: 'r-title' }, 'Title'), title, h('div', { class: 'hint' }, "A few words; each run's thread carries it on the board.")),
          h('div', { class: 'field' }, h('label', { for: 'r-prompt' }, 'What each run does'), prompt),
          h('div', { class: 'field' }, h('div', { class: 'label' }, 'Schedule'), seg, extra, preview),
          h(
            'div',
            { class: 'cols' },
            h('div', { class: 'field' }, h('label', { for: 'r-repo' }, 'Repository'), repo),
            h('div', { class: 'field' }, h('label', { for: 'r-model' }, 'Model'), model),
            h('div', { class: 'field' }, h('label', { for: 'r-effort' }, 'Effort'), effort),
          ),
          h(
            'div',
            { class: 'field check' },
            notify,
            h('label', { for: 'r-notify' }, 'Tell the coordinator when a run finishes', h('div', { class: 'hint', style: 'font-weight:400' }, 'It reads the report and tells you what it found. Off: runs only show on the board.')),
          ),
          error,
          h(
            'div',
            { class: 'form-actions' },
            h('button', { class: 'btn primary', type: 'submit', onclick: (e) => save(e.currentTarget) }, existing ? 'Save changes' : 'Add routine'),
            existing && h('button', { class: 'btn', type: 'button', onclick: (e) => runNow(e.currentTarget) }, 'Run now'),
            existing && toggle,
            h('span', { class: 'grow' }),
            existing && confirmButton('Delete', 'Delete it?', remove, 'danger ghost'),
            !existing && h('button', { class: 'btn ghost', type: 'button', onclick: () => select({ kind: 'coordinator' }) }, 'Cancel'),
          ),
        ),
        runs,
      );
      renderRuns();
      if (!existing) title.focus();
    }

    function renderRuns() {
      if (!$.runs || S.sel.kind !== 'routine') return;
      const rows = S.board.threads.filter((t) => t.routine === S.sel.name).sort((a, b) => b.last_activity_at - a.last_activity_at);
      fill($.runs, 
        h('h2', null, 'Runs'),
        rows.length ? rows.map(threadRow) : h('div', { class: 'empty' }, 'No run yet. It runs at its next time, or now with Run now.'),
      );
    }

    // -- Routines as things followed over time: verdicts, values, charts --

    const VERDICT = {
      attention: { label: 'Needs you', mark: '!', cls: 'v-attention' },
      update: { label: 'Update', mark: '•', cls: 'v-update' },
      nothing: { label: 'Nothing new', mark: '✓', cls: 'v-nothing' },
    };

    /** A verdict as an icon and a word, never colour alone. */
    function verdictBadge(finding) {
      const v = finding && VERDICT[finding.outcome];
      if (!v) return h('span', { class: 'verdict v-none' }, h('i', { 'aria-hidden': 'true' }, '–'), 'No run yet');
      return h('span', { class: `verdict ${v.cls}` }, h('i', { 'aria-hidden': 'true' }, v.mark), v.label);
    }

    /** 1,284 · 12.9K · 3.45 · with its unit. */
    function fmtNum(value, unit) {
      if (value === null || value === undefined) return '–';
      const abs = Math.abs(value);
      const text =
        abs >= 100000
          ? new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(value)
          : new Intl.NumberFormat(undefined, { maximumFractionDigits: abs >= 100 ? 1 : 2 }).format(value);
      if (!unit) return text;
      return unit === '%' ? `${text}%` : `${text} ${unit}`;
    }

    function delta(series) {
      if (series.previous === null || series.previous === undefined || series.latest === null) return null;
      const d = series.latest - series.previous;
      if (d === 0) return h('span', { class: 'delta' }, 'no change');
      // Whether up is good depends on the thing, which chief doesn't know: neutral ink.
      return h('span', { class: 'delta' }, `${d > 0 ? '▲' : '▼'} ${fmtNum(Math.abs(d), series.unit)} since the run before`);
    }

    const SVG = 'http://www.w3.org/2000/svg';
    function svg(tag, attrs, ...kids) {
      const el = document.createElementNS(SVG, tag);
      for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
      for (const kid of kids.flat()) if (kid) el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
      return el;
    }

    /** A tile's trend: the line in the quiet hue, the latest point in the series colour. */
    function sparkline(points) {
      const w = 120;
      const ht = 32;
      if (points.length < 2) return null;
      const values = points.map((p) => p[1]);
      const min = Math.min(...values);
      const span = Math.max(...values) - min || 1;
      const x = (i) => 3 + (i * (w - 6)) / (points.length - 1);
      const y = (v) => ht - 4 - ((v - min) / span) * (ht - 8);
      const path = points.map((p, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)} ${y(p[1]).toFixed(1)}`).join('');
      return svg(
        'svg',
        { class: 'spark', viewBox: `0 0 ${w} ${ht}`, width: w, height: ht, role: 'img', 'aria-label': `Trend over ${points.length} runs` },
        svg('path', { d: path, class: 'spark-line' }),
        svg('circle', { cx: x(points.length - 1), cy: y(values[values.length - 1]), r: 3, class: 'spark-dot' }),
      );
    }

    /** One series over time: a 2px line, a quiet grid, and a crosshair that reads the nearest run. */
    function lineChart(series) {
      const points = series.points;
      const wrap = h('div', { class: 'chart' });
      const draw = () => {
        const w = Math.max(280, wrap.clientWidth || 600);
        const ht = 180;
        const pad = { l: 52, r: 14, t: 10, b: 24 };
        const values = points.map((p) => p[1]);
        let min = Math.min(...values);
        let max = Math.max(...values);
        if (min === max) {
          min -= Math.abs(min) * 0.05 || 1;
          max += Math.abs(max) * 0.05 || 1;
        }
        const room = (max - min) * 0.12;
        min -= room;
        max += room;
        const t0 = points[0][0];
        const t1 = points[points.length - 1][0];
        const x = (t) => (t1 === t0 ? (pad.l + w - pad.r) / 2 : pad.l + ((t - t0) / (t1 - t0)) * (w - pad.l - pad.r));
        const y = (v) => ht - pad.b - ((v - min) / (max - min)) * (ht - pad.t - pad.b);
        // Round numbers inside the range: 700, 800, 900 rather than 684, 795.5, 907.
        const rough = (max - min) / 3;
        const mag = 10 ** Math.floor(Math.log10(rough));
        const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((m) => m >= rough) || rough;
        const ticks = [];
        for (let v = Math.ceil(min / step) * step; v <= max + step * 1e-9; v += step) ticks.push(Number(v.toPrecision(12)));
        const day = (ms) => new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });
        const hair = svg('line', { class: 'hair', y1: pad.t, y2: ht - pad.b, x1: 0, x2: 0, visibility: 'hidden' });
        const focus = svg('circle', { class: 'chart-dot', r: 4.5, cx: 0, cy: 0, visibility: 'hidden' });
        const last = points[points.length - 1];
        const plot = svg(
          'svg',
          { viewBox: `0 0 ${w} ${ht}`, width: w, height: ht, role: 'img', 'aria-label': `${series.name}: ${points.length} values, latest ${fmtNum(series.latest, series.unit)}` },
          ticks.map((v) => svg('line', { class: 'grid', x1: pad.l, x2: w - pad.r, y1: y(v), y2: y(v) })),
          ticks.map((v) => svg('text', { class: 'tick', x: pad.l - 8, y: y(v) + 4, 'text-anchor': 'end' }, fmtNum(v))),
          svg('text', { class: 'tick', x: pad.l, y: ht - 6 }, day(t0)),
          t1 !== t0 && svg('text', { class: 'tick', x: w - pad.r, y: ht - 6, 'text-anchor': 'end' }, day(t1)),
          points.length > 1 && svg('path', { class: 'chart-line', d: points.map((p, i) => `${i ? 'L' : 'M'}${x(p[0]).toFixed(1)} ${y(p[1]).toFixed(1)}`).join('') }),
          svg('circle', { class: 'chart-dot', r: 4.5, cx: x(last[0]), cy: y(last[1]) }),
          hair,
          focus,
        );
        const tip = h('div', { class: 'tip', hidden: true });
        const show = (event) => {
          const box = plot.getBoundingClientRect();
          const px = ((event.clientX - box.left) / box.width) * w;
          let near = points[0];
          for (const p of points) if (Math.abs(x(p[0]) - px) < Math.abs(x(near[0]) - px)) near = p;
          const cx = x(near[0]);
          hair.setAttribute('x1', cx);
          hair.setAttribute('x2', cx);
          hair.setAttribute('visibility', 'visible');
          focus.setAttribute('cx', cx);
          focus.setAttribute('cy', y(near[1]));
          focus.setAttribute('visibility', 'visible');
          fill(tip, h('b', null, fmtNum(near[1], series.unit)), h('span', null, stamp(near[0])));
          tip.hidden = false;
          const left = (cx / w) * box.width;
          tip.style.left = `${Math.min(Math.max(left, 70), box.width - 70)}px`;
          tip.style.top = `${(y(near[1]) / ht) * box.height}px`;
        };
        const hide = () => {
          hair.setAttribute('visibility', 'hidden');
          focus.setAttribute('visibility', 'hidden');
          tip.hidden = true;
        };
        plot.addEventListener('pointermove', show);
        plot.addEventListener('pointerdown', show);
        plot.addEventListener('pointerleave', hide);
        fill(wrap, plot, tip);
      };
      let drawn = 0;
      const redraw = () => {
        const width = wrap.clientWidth;
        if (!wrap.isConnected || width === drawn) return;
        drawn = width;
        draw();
      };
      draw();
      // Drawn at its real width, so text never stretches; again only when that width changes.
      new ResizeObserver(redraw).observe(wrap);
      const rows = [...points].reverse();
      return h(
        'section',
        { class: 'series' },
        h(
          'div',
          { class: 'series-head' },
          h('div', null, h('h3', null, series.name), delta(series)),
          h('div', { class: 'stat' }, fmtNum(series.latest, series.unit)),
        ),
        wrap,
        h(
          'details',
          { class: 'values' },
          h('summary', null, `All ${points.length} values`),
          h('table', null, h('tbody', null, rows.map((p) => h('tr', null, h('td', null, stamp(p[0])), h('td', { class: 'num' }, fmtNum(p[1], series.unit)))))),
        ),
      );
    }

    // -- Overview: everything the project follows, at a glance --

    function overviewPane(pane) {
      const b = S.board;
      const kids = [];
      const waiting = b.threads.filter((t) => t.group === 'waiting');
      const flagged = b.routines.filter((r) => r.last_finding && r.last_finding.outcome === 'attention');
      if (waiting.length || flagged.length) {
        kids.push(
          h('h2', { class: 'ov-title' }, 'Needs you'),
          h(
            'div',
            { class: 'needs' },
            waiting.map((t) =>
              h(
                'button',
                { class: 'need', onclick: () => select({ kind: 'thread', name: t.name }) },
                verdictBadge({ outcome: 'attention' }),
                h('b', null, t.title || t.name),
                h('span', null, t.asks ? `${t.asks.label} ${t.asks.text}` : t.waiting_reason === 'permission' ? 'asks for permission' : 'asked a question'),
              ),
            ),
            flagged.map((r) =>
              h(
                'button',
                { class: 'need', onclick: () => select({ kind: 'routine', name: r.name }, 'status') },
                verdictBadge(r.last_finding),
                h('b', null, r.title),
                h('span', null, r.last_finding.headline),
              ),
            ),
          ),
        );
      }
      kids.push(h('h2', { class: 'ov-title' }, 'Routines'));
      const tiles = b.routines.map((r) => {
        const f = r.last_finding;
        const main = r.series[0];
        return h(
          'button',
          { class: 'tile' + (r.enabled ? '' : ' off'), onclick: () => select({ kind: 'routine', name: r.name }) },
          h('div', { class: 'tile-head' }, h('b', null, r.title), verdictBadge(f)),
          h('p', { class: 'tile-line' }, f ? f.headline : r.enabled ? 'It hasn’t run yet.' : 'Paused.'),
          main &&
            h(
              'div',
              { class: 'tile-stat' },
              h('div', null, h('span', { class: 'stat-label' }, main.name), h('span', { class: 'stat' }, fmtNum(main.latest, main.unit))),
              sparkline(main.points),
            ),
          r.series.length > 1 && h('div', { class: 'tile-more' }, r.series.slice(1).map((s) => h('span', null, `${s.name} `, h('b', null, fmtNum(s.latest, s.unit))))),
          h(
            'div',
            { class: 'tile-foot' },
            h('span', null, r.schedule_text),
            h('span', null, !r.enabled ? 'paused' : r.next_run_at ? `next ${until(r.next_run_at)}` : ''),
            f && h('span', { title: stamp(f.at) }, `ran ${ago(f.at) === 'now' ? 'just now' : ago(f.at) + ' ago'}`),
          ),
        );
      });
      kids.push(
        tiles.length
          ? h('div', { class: 'tiles' }, tiles)
          : h('div', { class: 'blank' }, h('b', null, 'Nothing is followed yet'), 'Ask the coordinator to watch something on a schedule, or add a routine.'),
      );
      const working = b.threads.filter((t) => t.group === 'working').length;
      const open = b.threads.filter((t) => t.group !== 'resolved').length;
      kids.push(
        h('h2', { class: 'ov-title' }, 'Threads'),
        h(
          'p',
          { class: 'dim' },
          open ? `${open} open, ${working} working. ` : 'No open threads. ',
          h('a', { href: hashFor({ kind: 'coordinator' }, 'conversation') }, 'Talk to the coordinator'),
          ' to start one.',
        ),
      );
      fill(pane, h('div', { class: 'overview' }, kids));
    }

    // -- A routine's status: what it found, its values over time, what it remembers --

    async function loadRoutine(force) {
      const r = routine(S.sel.name);
      if (!r) return;
      const key = `${r.name}|${r.last_run_at}|${r.last_finding ? r.last_finding.at : ''}|${r.series.map((s) => s.at).join(',')}`;
      // Nothing it shows changed: leave the page, and whatever is hovered on it, alone.
      if (!force && key === S.routineKey && S.routineDetail) return false;
      S.routineKey = key;
      try {
        const detail = await api('GET', `${base}/routines/${encodeURIComponent(r.name)}`);
        if (S.routineKey === key) S.routineDetail = detail;
      } catch (e) {
        if (S.routineKey === key) S.routineDetail = { error: e.message };
      }
      return true;
    }

    function drawStatus() {
      if (S.sel.kind !== 'routine' || S.tab !== 'status' || !$.pane) return;
      const d = S.routineDetail;
      if (!d) return fill($.pane, h('div', { class: 'blank' }, 'Loading…'));
      if (d.error) return fill($.pane, h('div', { class: 'blank' }, d.error));
      const kids = [];
      const f = d.last_finding;
      if (f) {
        kids.push(
          h(
            'section',
            { class: 'finding ' + (VERDICT[f.outcome] ? VERDICT[f.outcome].cls : '') },
            h('div', { class: 'finding-head' }, verdictBadge(f), h('span', { class: 'dim', title: stamp(f.at) }, `${stamp(f.at)}`)),
            h('p', { class: 'finding-line' }, f.headline),
            f.details && h('div', { class: 'md', html: md(f.details) }),
            f.thread && h('a', { href: hashFor({ kind: 'thread', name: f.thread }, 'reports') }, 'Read the run’s report →'),
          ),
        );
      } else {
        kids.push(h('div', { class: 'blank' }, h('b', null, 'No run has reported yet'), d.enabled ? `It runs ${d.next_run_at ? until(d.next_run_at) : 'at its next time'}, or now from Settings.` : 'It is paused.'));
      }
      if (d.series.length) kids.push(h('div', { class: 'charts' }, d.series.map(lineChart)));
      if (d.findings.length > 1) {
        kids.push(
          h('h2', { class: 'ov-title' }, 'What its runs found'),
          h(
            'ul',
            { class: 'history' },
            d.findings.map((x) =>
              h(
                'li',
                null,
                h('time', { title: stamp(x.at) }, stamp(x.at)),
                verdictBadge(x),
                x.thread ? h('a', { href: hashFor({ kind: 'thread', name: x.thread }, 'reports') }, x.headline) : h('span', null, x.headline),
              ),
            ),
          ),
        );
      }
      const notes = String(d.notes || '').replace(/<!--[\s\S]*?-->/g, '').trim();
      kids.push(
        h('h2', { class: 'ov-title' }, 'What it remembers'),
        notes ? h('div', { class: 'notes md', html: md(notes) }) : h('p', { class: 'dim' }, 'Nothing yet: its runs keep their notes here.'),
        h('p', { class: 'dim small' }, h('code', null, d.notes_file), ' · each run reads this first and rewrites it. You can edit it; so can the coordinator.'),
      );
      fill($.pane, h('div', { class: 'inner wide' }, kids));
    }

    // -- Main: putting it together --

    /** Draws the main area for the selection and tab: called when either changes. */
    function renderMain() {
      closeTerm();
      form = null;
      $.runs = null;
      $.head = h('div', { class: 'main-head' });
      $.callouts = h('div');
      $.pane = h('div', { class: 'pane' });
      fill($.main, $.head, $.callouts, $.pane);
      renderHead();
      if (S.sel.kind === 'overview') {
        $.pane.className = 'pane padded';
        overviewPane($.pane);
        return;
      }
      if (S.sel.kind === 'routine' && S.tab === 'status' && routine(S.sel.name)) {
        $.pane.className = 'pane padded';
        S.routineDetail = null;
        S.routineKey = '';
        drawStatus();
        loadRoutine(true).then(drawStatus);
        return;
      }
      if (S.sel.kind === 'routine' || S.sel.kind === 'new-routine') {
        $.pane.className = 'pane padded';
        routinePane($.pane);
        return;
      }
      if (S.tab === 'conversation') {
        $.pane.className = 'pane';
        $.pane.style.display = 'flex';
        $.pane.style.flexDirection = 'column';
        $.pane.style.overflow = 'hidden';
        // On a phone the board is the home: attach only once the session is opened.
        if (wide() || S.view === 'detail') openTerm($.pane);
        return;
      }
      $.pane.className = 'pane padded';
      if (S.tab === 'project') {
        projectPane($.pane);
        return;
      }
      S.detail = null;
      S.detailKey = '';
      drawDetail();
      loadDetail(true).then(drawDetail);
    }

    function drawDetail() {
      if (S.tab === 'reports') fill($.pane, reportsPane());
      else if (S.tab === 'activity') fill($.pane, activityPane());
    }

    /** A new board arrived: redraw what shows it, and leave the terminal and forms alone. */
    function onBoard() {
      renderTop();
      renderBoard();
      if (!$.head) {
        apply();
        return;
      }
      // A tab that no longer applies (the thread was cleaned up while it showed).
      const s = current();
      const session = S.sel.kind === 'coordinator' || S.sel.kind === 'thread';
      if (session && !tabsFor(s).some(([k]) => k === S.tab)) {
        select(S.sel, defaultTab(S.sel));
        return;
      }
      if (S.sel.kind === 'overview') {
        renderHead();
        overviewPane($.pane);
        return;
      }
      if (S.sel.kind === 'routine' && S.tab === 'status' && routine(S.sel.name)) {
        renderHead();
        loadRoutine(false).then((changed) => changed && drawStatus());
        return;
      }
      // A routine made a moment ago wasn't on the board when its page was drawn.
      if (S.sel.kind === 'routine' && !form && routine(S.sel.name)) {
        renderMain();
        return;
      }
      renderHead();
      renderRuns();
      if (S.tab === 'reports' || S.tab === 'activity') loadDetail(false).then(drawDetail);
    }

    /** The hash changed (or the first board came): show what it names. */
    function apply() {
      if (!S.board) return;
      const at = readHash();
      const tab = at.tab || defaultTab(at.sel);
      const changed = !$.head || !sameSel(at.sel, S.sel) || tab !== S.tab;
      S.sel = at.sel;
      S.tab = tab;
      if (at.picked) S.view = 'detail';
      $.body.dataset.view = S.view;
      renderBoard();
      if (changed) renderMain();
      else if (S.tab === 'conversation' && !term && (wide() || S.view === 'detail')) openTerm($.pane);
    }

    // -- Start --

    shell();
    renderTop();
    api('GET', '/api/projects')
      .then((d) => {
        S.projects = d.projects;
        renderTop();
      })
      .catch(() => {});

    const stream = new EventSource(`${base}/stream`);
    stream.addEventListener('board', (event) => {
      const data = JSON.parse(event.data);
      S.live = true;
      if (data.gone) {
        stream.close();
        closeTerm();
        fill($app, h('div', { class: 'blank' }, h('b', null, 'This dashboard is not available'), data.gone, h('p', { style: 'margin-top:14px' }, h('a', { href: '/#all' }, 'All projects'))));
        return;
      }
      if (data.error && !data.project) {
        S.live = false;
        renderTop();
        return;
      }
      S.board = data;
      onBoard();
    });
    stream.onopen = () => {
      S.live = true;
      renderTop();
    };
    stream.onerror = () => {
      S.live = false;
      renderTop();
      // A stream refused outright (signed out, dashboard turned off) never reconnects.
      if (stream.readyState === EventSource.CLOSED) {
        api('GET', `${base}/board`).catch((e) => {
          fill($app, h('div', { class: 'blank' }, h('b', null, 'This dashboard is not available'), e.message, h('p', { style: 'margin-top:14px' }, h('a', { href: '/#all' }, 'All projects'))));
        });
      }
    };
    window.addEventListener('hashchange', apply);
    // Relative times move on their own.
    setInterval(() => {
      if (!S.board || document.hidden) return;
      renderBoard();
      if ($.head) renderHead();
    }, 30000);
  }

  // ---- Route ------------------------------------------------------------------------------

  const match = location.pathname.match(/^\/p\/([a-z0-9-]+)\/?$/);
  if (match) project(match[1]);
  else projectList();
})();
