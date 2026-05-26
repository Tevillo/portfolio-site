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
    pinned: false, // dropped nodes stay put; the rest relax around them
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
    // Spread scales with available area so the graph fills the pane rather than
    // huddling in a small square.
    repulsion = Math.max(8000, Math.min(30000, (W * H) / Math.max(nodes.length, 8) * 0.7));
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
  // Repulsion spreads nodes to fill the pane; springs hold links together; a
  // weak gravity keeps disconnected nodes from drifting off. Instead of a stiff
  // centre-seeking force (which "rubber-bands"), we recentre the whole layout's
  // centroid each frame so the graph stays put without springing back.
  const SPRING = 0.03; // edge stiffness
  const SPRING_LEN = 90; // edge rest length
  const GRAVITY = 0.004; // gentle containment for stray nodes
  const DAMPING = 0.9;
  let repulsion = 16000; // node-node Coulomb constant; scaled to the pane size
  let alpha = 1; // cools toward a low floor; reheats on interaction

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
        const d = Math.sqrt(d2);
        const force = repulsion / d2;
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

    let sx = 0;
    let sy = 0;
    let count = 0;
    for (const n of nodes) {
      // A node held by the pointer, or one the user dropped (pinned), is fixed:
      // it exerts forces on others but takes none itself, so nothing snaps back.
      if (n === dragNode || n.pinned) {
        n.vx = 0;
        n.vy = 0;
        continue;
      }
      n.vx += (cx - n.x) * GRAVITY;
      n.vy += (cy - n.y) * GRAVITY;
      n.vx *= DAMPING;
      n.vy *= DAMPING;
      n.x += n.vx * alpha;
      n.y += n.vy * alpha;
      sx += n.x;
      sy += n.y;
      count++;
    }

    // Recentre gently: ease the free nodes' centroid toward the pane centre a
    // fraction at a time. A full snap each frame makes dragging a hub slosh the
    // whole graph; easing keeps it calm. Fixed nodes are left untouched.
    if (count > 0) {
      const shiftX = (cx - sx / count) * 0.06;
      const shiftY = (cy - sy / count) * 0.06;
      for (const nd of nodes) {
        if (nd === dragNode || nd.pinned) continue;
        nd.x += shiftX;
        nd.y += shiftY;
      }
    }

    alpha *= 0.985;
    if (alpha < 0.02) alpha = 0.02;
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
      // A pinned (dropped) node gets an accent ring; others a surface outline.
      ctx.lineWidth = (n.pinned ? 2 : 1.5) / view.scale;
      ctx.strokeStyle = n.pinned ? C.accent : C.surface;
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
      dragNode.pinned = false; // picking it up frees it again
      alpha = Math.max(alpha, 0.3);
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
      alpha = Math.max(alpha, 0.25);
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
      // A click (no drag) opens the note.
      if (dragNode.url) window.location.href = dragNode.url;
    } else if (dragNode && moved) {
      // A drag drops the node where you let go and keeps it there.
      dragNode.pinned = true;
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
