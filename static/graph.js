// Obsidian-style force-directed graph of the vault, drawn on a canvas.
// Topology comes from the server (#graph-data); layout is simulated here.
(function () {
  const canvas = document.getElementById('graph-canvas');
  const dataEl = document.getElementById('graph-data');
  if (!canvas || !dataEl) return;

  let graph;
  try {
    graph = JSON.parse(dataEl.textContent || '{}');
  } catch (e) {
    return;
  }
  const nodes = (graph.nodes || []).map((n) => ({
    ...n,
    x: 0,
    y: 0,
    vx: 0,
    vy: 0,
    r: 4 + Math.sqrt(n.deg || 0) * 2.2,
  }));
  const edges = (graph.edges || []).map(([a, b]) => ({ a, b }));

  if (nodes.length === 0) {
    const empty = document.querySelector('.graph-empty');
    if (empty) empty.hidden = false;
    canvas.style.display = 'none';
    return;
  }

  // Read the site palette so the graph matches the rest of the page.
  const css = getComputedStyle(document.documentElement);
  const C = {
    bg: (css.getPropertyValue('--bg') || '#edefe7').trim(),
    surface: (css.getPropertyValue('--surface') || '#f6f7f1').trim(),
    text: (css.getPropertyValue('--text') || '#2f3a26').trim(),
    muted: (css.getPropertyValue('--muted') || '#6c7559').trim(),
    accent: (css.getPropertyValue('--accent') || '#6b8e4e').trim(),
    border: (css.getPropertyValue('--border') || '#c9d1b8').trim(),
  };

  const ctx = canvas.getContext('2d');
  let dpr = window.devicePixelRatio || 1;
  let W = 0;
  let H = 0;

  // View transform (pan/zoom), applied as screen = world * scale + offset.
  const view = { scale: 1, x: 0, y: 0 };

  function resize() {
    const rect = canvas.getBoundingClientRect();
    W = rect.width;
    H = rect.height;
    dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
  }

  // Seed nodes on a circle around the centre so the sim unfolds cleanly.
  function seed() {
    const cx = W / 2;
    const cy = H / 2;
    const radius = Math.min(W, H) * 0.3 + 1;
    nodes.forEach((n, i) => {
      const a = (i / nodes.length) * Math.PI * 2;
      n.x = cx + Math.cos(a) * radius;
      n.y = cy + Math.sin(a) * radius;
    });
  }

  // ---- Force simulation -------------------------------------------------
  const REPULSION = 9000; // node-node Coulomb constant
  const SPRING = 0.02; // edge stiffness
  const SPRING_LEN = 70; // edge rest length
  const GRAVITY = 0.015; // pull toward centre
  const DAMPING = 0.86;
  let alpha = 1; // cools to 0; reheats on interaction

  function step() {
    const cx = W / 2;
    const cy = H / 2;

    for (let i = 0; i < nodes.length; i++) {
      const a = nodes[i];
      for (let j = i + 1; j < nodes.length; j++) {
        const b = nodes[j];
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 0.01) {
          dx = Math.random() - 0.5;
          dy = Math.random() - 0.5;
          d2 = 0.01;
        }
        const force = REPULSION / d2;
        const d = Math.sqrt(d2);
        const fx = (dx / d) * force;
        const fy = (dy / d) * force;
        a.vx += fx;
        a.vy += fy;
        b.vx -= fx;
        b.vy -= fy;
      }
    }

    for (const e of edges) {
      const a = nodes[e.a];
      const b = nodes[e.b];
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
      const f = (d - SPRING_LEN) * SPRING;
      const fx = (dx / d) * f;
      const fy = (dy / d) * f;
      a.vx += fx;
      a.vy += fy;
      b.vx -= fx;
      b.vy -= fy;
    }

    for (const n of nodes) {
      n.vx += (cx - n.x) * GRAVITY;
      n.vy += (cy - n.y) * GRAVITY;
      if (n === dragNode) continue;
      n.vx *= DAMPING;
      n.vy *= DAMPING;
      n.x += n.vx * alpha;
      n.y += n.vy * alpha;
    }

    alpha *= 0.99;
    if (alpha < 0.03) alpha = 0.03;
  }

  // ---- Rendering --------------------------------------------------------
  function neighborsOf(idx) {
    const set = new Set();
    for (const e of edges) {
      if (e.a === idx) set.add(e.b);
      else if (e.b === idx) set.add(e.a);
    }
    return set;
  }

  let hoverIdx = -1;
  let activeNeighbors = new Set();

  function draw() {
    ctx.save();
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, W, H);
    ctx.translate(view.x, view.y);
    ctx.scale(view.scale, view.scale);

    const focused = hoverIdx >= 0;

    // Edges.
    ctx.lineWidth = 1 / view.scale;
    for (const e of edges) {
      const a = nodes[e.a];
      const b = nodes[e.b];
      const lit = focused && (e.a === hoverIdx || e.b === hoverIdx);
      ctx.strokeStyle = lit ? C.accent : C.border;
      ctx.globalAlpha = focused && !lit ? 0.25 : 0.7;
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;

    // Nodes.
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      const lit = !focused || i === hoverIdx || activeNeighbors.has(i);
      ctx.globalAlpha = lit ? 1 : 0.3;
      ctx.beginPath();
      ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
      ctx.fillStyle = i === hoverIdx ? C.accent : C.muted;
      ctx.fill();
      ctx.lineWidth = 1.5 / view.scale;
      ctx.strokeStyle = C.surface;
      ctx.stroke();
    }

    // Labels: always for high-degree nodes; otherwise when zoomed in or focused.
    ctx.globalAlpha = 1;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    const fontPx = Math.max(9, 11 / view.scale);
    ctx.font = `${fontPx}px ui-sans-serif, system-ui, sans-serif`;
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      const show =
        i === hoverIdx ||
        activeNeighbors.has(i) ||
        view.scale > 1.1 ||
        n.deg >= 4;
      if (!show) continue;
      const lit = !focused || i === hoverIdx || activeNeighbors.has(i);
      ctx.globalAlpha = lit ? 1 : 0.3;
      ctx.fillStyle = C.text;
      ctx.fillText(n.label, n.x, n.y + n.r + 2);
    }
    ctx.globalAlpha = 1;
    ctx.restore();
  }

  function loop() {
    step();
    draw();
    requestAnimationFrame(loop);
  }

  // ---- Interaction ------------------------------------------------------
  function toWorld(sx, sy) {
    return {
      x: (sx - view.x) / view.scale,
      y: (sy - view.y) / view.scale,
    };
  }

  function nodeAt(sx, sy) {
    const p = toWorld(sx, sy);
    for (let i = nodes.length - 1; i >= 0; i--) {
      const n = nodes[i];
      const dx = p.x - n.x;
      const dy = p.y - n.y;
      const hit = n.r + 4 / view.scale;
      if (dx * dx + dy * dy <= hit * hit) return i;
    }
    return -1;
  }

  let dragNode = null;
  let panning = false;
  let moved = false;
  let last = { x: 0, y: 0 };
  let downAt = { x: 0, y: 0 };

  function pos(e) {
    const rect = canvas.getBoundingClientRect();
    return { x: e.clientX - rect.left, y: e.clientY - rect.top };
  }

  canvas.addEventListener('mousedown', (e) => {
    const p = pos(e);
    downAt = p;
    moved = false;
    last = p;
    const idx = nodeAt(p.x, p.y);
    if (idx >= 0) {
      dragNode = nodes[idx];
      alpha = Math.max(alpha, 0.6);
    } else {
      panning = true;
    }
  });

  window.addEventListener('mousemove', (e) => {
    const p = pos(e);
    if (Math.abs(p.x - downAt.x) + Math.abs(p.y - downAt.y) > 4) moved = true;

    if (dragNode) {
      const w = toWorld(p.x, p.y);
      dragNode.x = w.x;
      dragNode.y = w.y;
      dragNode.vx = 0;
      dragNode.vy = 0;
      alpha = Math.max(alpha, 0.4);
      return;
    }
    if (panning) {
      view.x += p.x - last.x;
      view.y += p.y - last.y;
      last = p;
      return;
    }
    // Hover.
    const idx = nodeAt(p.x, p.y);
    if (idx !== hoverIdx) {
      hoverIdx = idx;
      activeNeighbors = idx >= 0 ? neighborsOf(idx) : new Set();
      canvas.style.cursor = idx >= 0 ? 'pointer' : 'grab';
    }
  });

  window.addEventListener('mouseup', (e) => {
    if (dragNode && !moved) {
      const idx = nodes.indexOf(dragNode);
      if (idx >= 0 && nodes[idx].url) window.location.href = nodes[idx].url;
    }
    dragNode = null;
    panning = false;
  });

  canvas.addEventListener(
    'wheel',
    (e) => {
      e.preventDefault();
      const p = pos(e);
      const factor = e.deltaY < 0 ? 1.1 : 1 / 1.1;
      const ns = Math.min(4, Math.max(0.25, view.scale * factor));
      // Zoom toward the cursor.
      view.x = p.x - (p.x - view.x) * (ns / view.scale);
      view.y = p.y - (p.y - view.y) * (ns / view.scale);
      view.scale = ns;
    },
    { passive: false }
  );

  window.addEventListener('resize', () => {
    resize();
  });

  resize();
  seed();
  loop();
})();
