// Webview UI for the code graph. Talks to src/graphPanel.ts via postMessage.
// The extension computes the views; this file only draws them and handles interaction.
(function () {
  'use strict';

  const vscode = acquireVsCodeApi();
  if (window.cytoscapeFcose) {
    cytoscape.use(window.cytoscapeFcose);
  }
  const $ = (id) => document.getElementById(id);

  const ALL_EDGE_KINDS = ['calls', 'imports', 'references', 'inherits', 'contains'];
  const NODE_COLORS = {
    dir: '#7f8c8d',
    file: '#4e79a7',
    external: '#9c755f',
    module: '#76b7b2',
    class: '#f28e2b',
    struct: '#f28e2b',
    enum: '#edc948',
    interface: '#b07aa1',
    type: '#ff9da7',
    function: '#59a14f',
    method: '#8cd17d',
    constant: '#bab0ac',
    macro: '#e15759',
  };
  const EDGE_COLORS = {
    calls: '#e8a33d',
    imports: '#4e79a7',
    references: '#9467bd',
    inherits: '#e15759',
    contains: '#8a8a8a',
  };
  // Categorical palette for communities (Tableau 10, then golden-angle hues).
  const COMMUNITY_COLORS = ['#4e79a7', '#f28e2b', '#e15759', '#76b7b2', '#59a14f',
    '#edc948', '#b07aa1', '#ff9da7', '#9c755f', '#bab0ac'];
  const NO_COMMUNITY = '#888888';
  const LEGEND_MAX = 12;

  function communityColor(id) {
    if (id === undefined || id === null) {
      return NO_COMMUNITY;
    }
    if (id < COMMUNITY_COLORS.length) {
      return COMMUNITY_COLORS[id];
    }
    const hue = Math.round((id * 137.508) % 360);
    return `hsl(${hue}, 55%, 55%)`;
  }

  const INVERSE = {
    contains: 'contained by',
    imports: 'imported by',
    calls: 'called by',
    inherits: 'inherited by',
    references: 'referenced by',
  };

  const defaults = {
    params: { mode: 'modules', edgeKinds: ALL_EDGE_KINDS.slice(), showExternal: false, direction: 'both' },
    depths: { modules: 2, focus: 1 },
    layout: window.cytoscapeFcose ? 'fcose' : 'cose',
    colorBy: 'community',
    history: [],
  };
  const state = Object.assign({}, defaults, vscode.getState() || {});
  let lastRequest = '';
  let selectedId = null;
  let info = null;
  let currentView = null;
  let highlighted = null;

  /** Colour by community only when the graph has communities (format v2). */
  function byCommunity() {
    return state.colorBy === 'community' && !!(currentView && currentView.communities);
  }

  function nodeColor(e) {
    const kind = e.data('kind');
    if (!byCommunity() || kind === 'external') {
      return NODE_COLORS[kind] || '#888';
    }
    return communityColor(e.data('community'));
  }

  /** Directory groups stay neutral when colouring by community. */
  function groupColor(e) {
    return byCommunity() ? NO_COMMUNITY : NODE_COLORS[e.data('kind')] || '#888';
  }

  // ---------- Cytoscape ----------

  const cy = cytoscape({
    container: $('cy'),
    wheelSensitivity: 0.3,
    minZoom: 0.05,
    maxZoom: 4,
    boxSelectionEnabled: false,
    style: buildStyle(),
  });

  function cssVar(name, fallback) {
    const v = getComputedStyle(document.body).getPropertyValue(name).trim();
    return v || fallback;
  }

  function buildStyle() {
    const fg = cssVar('--vscode-editor-foreground', '#cccccc');
    const bg = cssVar('--vscode-editor-background', '#1e1e1e');
    const focus = cssVar('--vscode-focusBorder', '#007fd4');
    const font = cssVar('--vscode-font-family', 'sans-serif');
    return [
      {
        selector: 'node',
        style: {
          label: 'data(label)',
          'font-family': font,
          'font-size': 11,
          'min-zoomed-font-size': 6,
          color: fg,
          'text-valign': 'bottom',
          'text-margin-y': 4,
          'text-outline-color': bg,
          'text-outline-width': 2,
          'background-color': nodeColor,
          width: nodeSize,
          height: nodeSize,
          'border-width': 0,
          shape: (e) => nodeShape(e.data('kind')),
        },
      },
      {
        selector: ':parent',
        style: {
          'background-color': groupColor,
          'background-opacity': 0.08,
          'border-width': 1,
          'border-color': groupColor,
          'border-opacity': 0.6,
          shape: 'round-rectangle',
          'text-valign': 'top',
          'text-halign': 'center',
          'text-margin-y': -4,
          'font-size': 11,
          'font-weight': 'bold',
          padding: 14,
        },
      },
      { selector: 'node[?dim]', style: { opacity: 0.45 } },
      {
        selector: 'node[?focus]',
        style: { 'border-width': 4, 'border-color': focus, 'font-weight': 'bold', 'font-size': 13 },
      },
      {
        selector: 'node:selected',
        style: { 'border-width': 3, 'border-color': focus, 'border-opacity': 1 },
      },
      {
        selector: 'edge',
        style: {
          width: (e) => 1 + Math.min(5, Math.log2(e.data('count') || 1)),
          'line-color': (e) => EDGE_COLORS[e.data('kind')] || '#888',
          'target-arrow-color': (e) => EDGE_COLORS[e.data('kind')] || '#888',
          'target-arrow-shape': 'triangle',
          'arrow-scale': 0.8,
          'curve-style': 'bezier',
          opacity: 0.75,
        },
      },
      { selector: 'edge[?ambiguous]', style: { 'line-style': 'dashed' } },
      { selector: 'edge[kind = "contains"]', style: { 'line-style': 'dotted', 'target-arrow-shape': 'none' } },
      { selector: '.faded, .cfaded', style: { opacity: 0.12 } },
      { selector: 'edge.hl', style: { opacity: 1, label: 'data(label)', 'font-size': 10, color: fg,
        'text-background-color': bg, 'text-background-opacity': 0.8, 'text-background-padding': 2,
        'font-family': font } },
    ];
  }

  function nodeSize(e) {
    if (e.data('focus')) {
      return 34;
    }
    const kind = e.data('kind');
    const size = e.data('size') || 0;
    if (kind === 'dir') {
      return 24 + Math.min(56, Math.sqrt(size) * 8);
    }
    if (kind === 'file') {
      return 16 + Math.min(30, Math.sqrt(size) * 4);
    }
    return 18;
  }

  function nodeShape(kind) {
    switch (kind) {
      case 'dir': return 'round-rectangle';
      case 'file': return 'round-rectangle';
      case 'external': return 'diamond';
      case 'interface': return 'hexagon';
      case 'class':
      case 'struct':
      case 'enum':
      case 'type': return 'round-tag';
      case 'constant': return 'triangle';
      case 'macro': return 'star';
      default: return 'ellipse';
    }
  }

  // Re-read theme colours when VS Code switches theme (it changes the body class).
  new MutationObserver(() => cy.style(buildStyle())).observe(document.body, {
    attributes: true,
    attributeFilter: ['class'],
  });

  // ---------- Rendering ----------

  function render(view) {
    currentView = view;
    selectedId = null;
    hideDetails();
    cy.elements().remove();
    const elements = [];
    for (const n of view.nodes) {
      elements.push({ group: 'nodes', data: Object.assign({}, n) });
    }
    for (const e of view.edges) {
      const label = e.count > 1 ? `${e.kind} ×${e.count}` : e.kind;
      elements.push({ group: 'edges', data: Object.assign({ label }, e) });
    }
    cy.add(elements);
    highlighted = null;
    cy.style(buildStyle());
    renderLegend();

    const empty = $('empty');
    if (view.nodes.length === 0) {
      empty.hidden = false;
      empty.textContent =
        state.params.mode === 'focus'
          ? state.params.center
            ? `"${state.params.center}" is not in the graph (re-indexed?). Search for a symbol above.`
            : 'Search for a symbol, or double-click a node in another view to focus on it.'
          : 'Nothing to show with the current filters.';
    } else {
      empty.hidden = true;
    }
    runLayout();
    updateStatus();
  }

  function runLayout() {
    if (cy.nodes().length === 0) {
      return;
    }
    const name = state.layout;
    const focus = cy.nodes('[?focus]');
    const opts = { name, animate: false, fit: true, padding: 30, nodeDimensionsIncludeLabels: true };
    if (name === 'fcose') {
      Object.assign(opts, {
        quality: cy.nodes().length > 300 ? 'default' : 'proof',
        randomize: true,
        nodeRepulsion: () => 6500,
        idealEdgeLength: () => 80,
        edgeElasticity: () => 0.45,
        nestingFactor: 0.1,
        gravity: 0.25,
        gravityCompound: 1.0,
        tilingPaddingVertical: 20,
        tilingPaddingHorizontal: 20,
        packComponents: true,
      });
    } else if (name === 'cose') {
      Object.assign(opts, {
        idealEdgeLength: () => 90,
        nodeRepulsion: () => 9000,
        nodeOverlap: 12,
        componentSpacing: 60,
        gravity: 0.6,
        numIter: cy.nodes().length > 250 ? 600 : 1000,
        randomize: true,
      });
    } else if (name === 'breadthfirst') {
      Object.assign(opts, { directed: true, spacingFactor: 1.1 });
      if (focus.length) {
        opts.roots = focus;
      }
    } else if (name === 'concentric') {
      Object.assign(opts, {
        concentric: (n) => (n.data('focus') ? 1000 : n.degree()),
        levelWidth: () => 3,
        minNodeSpacing: 20,
      });
    }
    cy.layout(opts).run();
  }

  // ---------- Navigation & requests ----------

  function save() {
    vscode.setState(state);
  }

  function request() {
    const params = Object.assign({}, state.params, {
      depth: state.depths[state.params.mode],
    });
    lastRequest = JSON.stringify(params);
    $('status').textContent = 'Loading…';
    vscode.postMessage({ type: 'requestView', params });
  }

  function go(changes, push = true) {
    changes = Object.assign({}, changes);
    if (changes.depth !== undefined) {
      state.depths[changes.mode || state.params.mode] = changes.depth;
      delete changes.depth;
    }
    const next = Object.assign({}, state.params, changes);
    if (JSON.stringify(next) === JSON.stringify(state.params)) {
      request();
      return;
    }
    if (push) {
      state.history.push(state.params);
      if (state.history.length > 50) {
        state.history.shift();
      }
    }
    state.params = next;
    save();
    syncToolbar();
    request();
  }

  function back() {
    const prev = state.history.pop();
    if (prev) {
      state.params = prev;
      save();
      syncToolbar();
      request();
    }
  }

  function syncToolbar() {
    const p = state.params;
    document.querySelectorAll('#modes button').forEach((b) => {
      b.classList.toggle('active', b.dataset.mode === p.mode);
      b.setAttribute('aria-selected', String(b.dataset.mode === p.mode));
    });
    $('back').disabled = state.history.length === 0;
    $('depthField').hidden = p.mode === 'files';
    $('depth').value = state.depths[p.mode] || 1;
    $('directionField').hidden = p.mode !== 'focus';
    $('direction').value = p.direction || 'both';
    $('layout').value = state.layout;
    $('colorBy').value = state.colorBy;
    $('showExternal').checked = !!p.showExternal;
    document.querySelectorAll('#edgeKinds input[value]').forEach((cb) => {
      cb.checked = p.edgeKinds.includes(cb.value);
      // Containment is drawn as nesting outside of Focus.
      if (cb.value === 'contains') {
        cb.disabled = p.mode !== 'focus';
        cb.parentElement.classList.toggle('disabled', cb.disabled);
      }
    });
  }

  function updateStatus() {
    const parts = [];
    if (currentView) {
      parts.push(currentView.title);
      const parents = new Set(currentView.nodes.map((n) => n.parent).filter(Boolean));
      const leaves = currentView.nodes.filter((n) => !parents.has(n.id));
      parts.push(`${leaves.length} nodes, ${currentView.edges.length} edges`);
      if (currentView.truncated) {
        parts.push(`⚠ showing ${currentView.truncated.shown} of ${currentView.truncated.total} (codenav.maxNodes)`);
      }
    }
    if (info) {
      parts.push(`indexed ${timeAgo(info.indexedAt)}`);
    }
    $('status').textContent = parts.join(' · ');
  }

  function timeAgo(seconds) {
    const diff = Math.max(0, Date.now() / 1000 - seconds);
    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.round(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.round(diff / 3600)} h ago`;
    return new Date(seconds * 1000).toLocaleDateString();
  }

  // ---------- Graph interaction ----------

  cy.on('tap', 'node', (evt) => {
    const n = evt.target;
    select(n);
  });

  cy.on('tap', (evt) => {
    if (evt.target === cy) {
      clearHighlight();
      hideDetails();
    }
  });

  cy.on('dbltap', 'node', (evt) => drillDown(evt.target));

  cy.on('mouseover', 'node', (evt) => {
    let tip = evt.target.data('tooltip');
    if (!tip) return;
    const label = communityLabel(evt.target.data('community'));
    if (label !== undefined) {
      tip += `\ncommunity ${evt.target.data('community')}: ${label}`;
    }
    const t = $('tooltip');
    t.textContent = tip;
    t.hidden = false;
  });
  cy.on('mousemove', (evt) => {
    const t = $('tooltip');
    if (!t.hidden && evt.renderedPosition) {
      t.style.left = evt.renderedPosition.x + 14 + 'px';
      t.style.top = evt.renderedPosition.y + 14 + 'px';
    }
  });
  cy.on('mouseout', 'node', () => {
    $('tooltip').hidden = true;
  });

  function select(n) {
    selectedId = n.id();
    highlight(n);
    const ref = n.data('ref');
    if (ref) {
      vscode.postMessage({ type: 'details', id: ref });
    } else {
      showGroupDetails(n);
    }
  }

  function highlight(n) {
    clearHighlight();
    // Keep the node, its nested children, their edges and the nodes at the other end (with
    // their enclosing groups); fade everything else.
    const edges = n.connectedEdges().union(n.descendants().connectedEdges());
    let keep = n.union(n.descendants()).union(edges).union(edges.connectedNodes());
    keep = keep.union(keep.ancestors());
    cy.elements().not(keep).addClass('faded');
    edges.addClass('hl');
  }

  function clearHighlight() {
    cy.elements().removeClass('faded hl');
  }

  function drillDown(n) {
    const kind = n.data('kind');
    if (kind === 'dir') {
      go({ mode: 'files', dir: n.data('path') });
    } else if (n.data('ref')) {
      go({ mode: 'focus', center: n.data('ref') });
    }
  }

  // ---------- Details panel ----------

  function esc(s) {
    return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  function showGroupDetails(n) {
    const children = n.children().length;
    const panel = $('details');
    panel.innerHTML = `
      <div class="d-head"><span class="kind" style="background:${NODE_COLORS.dir}">dir</span>
        <button class="close" title="Close">×</button></div>
      <h3>${esc(n.data('path') || n.data('label'))}</h3>
      ${n.data('size') ? `<p class="muted">${n.data('size')} files</p>` : ''}
      ${children ? `<p class="muted">${children} items shown</p>` : ''}
      <div class="actions"><button data-act="files">Show files</button></div>`;
    panel.hidden = false;
    panel.querySelector('.close').onclick = hideDetails;
    panel.querySelector('[data-act="files"]').onclick = () => go({ mode: 'files', dir: n.data('path') });
  }

  function showDetails(d) {
    if (!d) return;
    const panel = $('details');
    const loc = d.file ? `${esc(d.file)}${d.line ? `:${d.line}` : ''}` : '';
    const rel = (counts, dir) =>
      Object.entries(counts)
        .sort((a, b) => b[1] - a[1])
        .map(
          ([k, c]) =>
            `<li><a href="#" data-kind="${k}" data-dir="${dir}"><i class="swatch e-${k}"></i>${
              dir === 'out' ? k : INVERSE[k] || k
            }</a><span>${c}</span></li>`,
        )
        .join('');
    panel.innerHTML = `
      <div class="d-head"><span class="kind" style="background:${NODE_COLORS[d.kind] || '#888'}">${esc(d.kind)}</span>
        ${d.lang ? `<span class="muted">${esc(d.lang)}</span>` : ''}
        <button class="close" title="Close">×</button></div>
      <h3>${esc(d.name)}</h3>
      <div class="muted id">${esc(d.id)}</div>
      ${loc ? `<div class="loc"><a href="#" data-act="open">${loc}</a></div>` : ''}
      ${d.community ? `<div class="community"><i class="dot" style="background:${communityColor(d.community.id)}"></i>community ${d.community.id}: ${esc(d.community.label)}</div>` : ''}
      ${d.signature ? `<pre>${esc(d.signature)}</pre>` : ''}
      ${d.doc ? `<p class="doc">${esc(d.doc)}</p>` : ''}
      <div class="actions">
        ${d.file ? '<button data-act="open">Open</button>' : ''}
        <button data-act="focus">Focus</button>
      </div>
      ${Object.keys(d.outgoing).length ? `<h4>Outgoing</h4><ul class="rels">${rel(d.outgoing, 'out')}</ul>` : ''}
      ${Object.keys(d.incoming).length ? `<h4>Incoming</h4><ul class="rels">${rel(d.incoming, 'in')}</ul>` : ''}`;
    panel.hidden = false;
    panel.querySelector('.close').onclick = hideDetails;
    panel.querySelectorAll('[data-act="open"]').forEach((el) => {
      el.onclick = (e) => {
        e.preventDefault();
        vscode.postMessage({ type: 'open', id: d.id });
      };
    });
    panel.querySelector('[data-act="focus"]').onclick = () => go({ mode: 'focus', center: d.id });
    // Clicking a relation focuses on exactly that relation, e.g. "called by" → incoming calls.
    panel.querySelectorAll('.rels a').forEach((el) => {
      el.onclick = (e) => {
        e.preventDefault();
        go({ mode: 'focus', center: d.id, direction: el.dataset.dir, edgeKinds: [el.dataset.kind] });
      };
    });
  }

  function hideDetails() {
    $('details').hidden = true;
  }

  // ---------- Community legend ----------

  function renderLegend() {
    const legend = $('legend');
    const list = (currentView && currentView.communities) || [];
    $('colorField').hidden = list.length === 0;
    if (!byCommunity() || list.length === 0) {
      legend.hidden = true;
      return;
    }
    // Most visible communities first.
    const shown = new Map();
    cy.nodes().forEach((n) => {
      const c = n.data('community');
      if (c !== undefined && n.isChildless()) {
        shown.set(c, (shown.get(c) || 0) + 1);
      }
    });
    const items = list.slice().sort((a, b) => (shown.get(b.id) || 0) - (shown.get(a.id) || 0) || a.id - b.id);
    const rows = items
      .slice(0, LEGEND_MAX)
      .map(
        (c) => `<li><a href="#" data-community="${c.id}" class="${highlighted === c.id ? 'active' : ''}"
          title="${esc(c.label)}: ${c.size} nodes in the repository, ${shown.get(c.id) || 0} shown">
          <i class="dot" style="background:${communityColor(c.id)}"></i>${esc(c.label)}</a></li>`,
      )
      .join('');
    const more = items.length > LEGEND_MAX ? `<li class="muted">+${items.length - LEGEND_MAX} more</li>` : '';
    legend.innerHTML = `<div class="muted">Communities</div><ul>${rows}${more}</ul>`;
    legend.hidden = false;
    legend.querySelectorAll('a[data-community]').forEach((a) => {
      a.onclick = (e) => {
        e.preventDefault();
        highlightCommunity(Number(a.dataset.community));
      };
    });
  }

  /** Fades everything outside community `id`; the same id again clears it. */
  function highlightCommunity(id) {
    highlighted = highlighted === id ? null : id;
    cy.elements().removeClass('cfaded');
    if (highlighted !== null) {
      const members = cy.nodes().filter((n) => n.data('community') === highlighted);
      const keep = members.union(members.ancestors()).union(members.edgesWith(members));
      cy.elements().not(keep).addClass('cfaded');
    }
    renderLegend();
  }

  function communityLabel(id) {
    const c = currentView && currentView.communities && currentView.communities.find((x) => x.id === id);
    return c ? c.label : undefined;
  }

  // ---------- Search ----------

  let searchTimer;
  let results = [];
  let activeResult = -1;

  $('search').addEventListener('input', (e) => {
    clearTimeout(searchTimer);
    const q = e.target.value;
    if (!q.trim()) {
      closeResults();
      return;
    }
    searchTimer = setTimeout(() => vscode.postMessage({ type: 'search', query: q }), 120);
  });

  $('search').addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (!results.length) return;
      activeResult = (activeResult + (e.key === 'ArrowDown' ? 1 : -1) + results.length) % results.length;
      paintResults();
    } else if (e.key === 'Enter') {
      const r = results[activeResult >= 0 ? activeResult : 0];
      if (r) pickResult(r);
    } else if (e.key === 'Escape') {
      closeResults();
    }
  });

  $('search').addEventListener('blur', () => setTimeout(closeResults, 150));

  function showResults(query, list) {
    if (query !== $('search').value) return;
    results = list;
    activeResult = list.length ? 0 : -1;
    paintResults();
  }

  function paintResults() {
    const ul = $('results');
    if (!results.length) {
      ul.innerHTML = '<li class="muted">No matches</li>';
    } else {
      ul.innerHTML = results
        .map(
          (r, i) => `<li data-i="${i}" class="${i === activeResult ? 'active' : ''}">
            <span class="kind" style="background:${NODE_COLORS[r.kind] || '#888'}">${esc(r.kind)}</span>
            <b>${esc(r.name)}</b><span class="muted">${esc(r.file ? `${r.file}:${r.line}` : r.id)}</span></li>`,
        )
        .join('');
      ul.querySelectorAll('li[data-i]').forEach((li) => {
        li.onmousedown = (e) => {
          e.preventDefault();
          pickResult(results[Number(li.dataset.i)]);
        };
      });
    }
    ul.hidden = false;
  }

  function pickResult(r) {
    closeResults();
    $('search').value = '';
    go({ mode: 'focus', center: r.id });
  }

  function closeResults() {
    $('results').hidden = true;
    results = [];
    activeResult = -1;
  }

  // ---------- Toolbar ----------

  document.querySelectorAll('#modes button').forEach((b) => {
    b.onclick = () => {
      const changes = { mode: b.dataset.mode };
      if (b.dataset.mode === 'files') changes.dir = undefined;
      go(changes);
    };
  });
  $('back').onclick = back;
  $('fit').onclick = () => cy.fit(undefined, 30);
  $('reindex').onclick = () => vscode.postMessage({ type: 'reindex' });
  $('depth').onchange = (e) => {
    const v = Math.max(1, Math.min(6, Number(e.target.value) || 1));
    state.depths[state.params.mode] = v;
    save();
    request();
  };
  $('direction').onchange = (e) => go({ direction: e.target.value }, false);
  $('layout').onchange = (e) => {
    state.layout = e.target.value;
    save();
    runLayout();
  };
  $('colorBy').onchange = (e) => {
    state.colorBy = e.target.value;
    save();
    cy.style(buildStyle());
    renderLegend();
  };
  $('showExternal').onchange = (e) => go({ showExternal: e.target.checked }, false);
  document.querySelectorAll('#edgeKinds input[value]').forEach((cb) => {
    cb.onchange = () => {
      const kinds = [...document.querySelectorAll('#edgeKinds input[value]:checked')].map((c) => c.value);
      go({ edgeKinds: kinds }, false);
    };
  });
  window.addEventListener('keydown', (e) => {
    if ((e.altKey && e.key === 'ArrowLeft') || (e.key === 'Backspace' && document.activeElement === document.body)) {
      e.preventDefault();
      back();
    } else if ((e.metaKey || e.ctrlKey) && e.key === 'f') {
      e.preventDefault();
      $('search').focus();
    }
  });

  // ---------- Messages from the extension ----------

  window.addEventListener('message', (event) => {
    const m = event.data;
    switch (m.type) {
      case 'config':
        if (!vscode.getState()) {
          state.depths.modules = m.moduleDepth;
          syncToolbar();
        }
        break;
      case 'info':
        info = m;
        updateStatus();
        break;
      case 'graphChanged':
        request();
        break;
      case 'navigate':
        go(m.params);
        break;
      case 'view':
        // Ignore stale responses from an earlier request.
        if (JSON.stringify(m.params) === lastRequest) {
          render(m.view);
        }
        break;
      case 'details':
        if (m.details && selectedId && cy.getElementById(selectedId).data('ref') === m.details.id) {
          showDetails(m.details);
        }
        break;
      case 'searchResults':
        showResults(m.query, m.results);
        break;
      case 'error':
        $('status').textContent = '⚠ ' + m.message;
        if (!currentView || currentView.nodes.length === 0) {
          $('empty').hidden = false;
          $('empty').textContent = m.message;
        }
        break;
    }
  });

  syncToolbar();
  vscode.postMessage({ type: 'ready' });
})();
