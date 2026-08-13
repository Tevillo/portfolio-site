(function () {
  // Tri-state storage: each path is either explicitly "open", explicitly
  // "closed", or unset. Unset paths fall back to the server-rendered default
  // (data-default-collapsed=true | false). The old single-array format from
  // earlier deploys is migrated as "closed" entries on first read.
  const STORAGE_KEY = 'portfolio-folder-state';
  const LEGACY_KEY = 'portfolio-collapsed-folders';
  let state = {}; // path -> 'open' | 'closed'

  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed === 'object') state = parsed;
    } else {
      const legacy = localStorage.getItem(LEGACY_KEY);
      if (legacy) {
        for (const p of JSON.parse(legacy)) state[p] = 'closed';
        localStorage.removeItem(LEGACY_KEY);
      }
    }
  } catch (e) { /* ignore */ }

  const sections = Array.from(document.querySelectorAll('section.gallery[data-path]'));
  // Only /all renders a tree; the work and portfolio pages have sections alone
  // and everything below degrades to the original per-section behaviour.
  const sidebar = document.querySelector('.tree-sidebar');
  const treeNodes = Array.from(document.querySelectorAll('li.tree-node[data-path]'));
  if (!sections.length && !treeNodes.length) return;

  // Every collapsible path on the page, with its server-rendered default. The
  // tree contributes container folders that own no section at all, which is
  // what lets a year be folded even though it holds no photos of its own.
  const defaults = new Map();
  for (const sec of sections) {
    defaults.set(sec.dataset.path, sec.dataset.defaultCollapsed === 'true');
  }
  for (const node of treeNodes) {
    if (!defaults.has(node.dataset.path)) defaults.set(node.dataset.path, false);
  }
  const paths = Array.from(defaults.keys());

  function persist() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
    } catch (e) { /* ignore */ }
  }

  function isClosed(path) {
    const explicit = state[path];
    if (explicit === 'open') return false;
    if (explicit === 'closed') return true;
    return defaults.get(path) === true;
  }

  // True when `path` is a strict descendant of `ancestor`. The root folder has
  // an empty path and is an ancestor of everything.
  function isUnder(path, ancestor) {
    if (path === ancestor) return false;
    return ancestor === '' || path.startsWith(ancestor + '/');
  }

  function ancestorClosed(path) {
    for (const p of paths) {
      if (isUnder(path, p) && isClosed(p)) return true;
    }
    return false;
  }

  function apply() {
    for (const sec of sections) {
      const path = sec.dataset.path;
      const closed = isClosed(path);
      sec.classList.toggle('collapsed', closed);
      sec.classList.toggle('hidden-by-ancestor', ancestorClosed(path));
      const btn = sec.querySelector('.collapse-toggle');
      if (btn) {
        btn.setAttribute('aria-expanded', closed ? 'false' : 'true');
        btn.setAttribute('aria-label', closed ? 'Expand folder' : 'Collapse folder');
      }
    }
    for (const node of treeNodes) {
      const closed = isClosed(node.dataset.path);
      node.classList.toggle('collapsed', closed);
      const twisty = node.querySelector(':scope > .tree-row > button.tree-twisty');
      if (twisty) {
        twisty.setAttribute('aria-expanded', closed ? 'false' : 'true');
        const name = node.dataset.name || 'folder';
        twisty.setAttribute('aria-label', (closed ? 'Expand ' : 'Collapse ') + name);
      }
    }
  }

  function toggle(path) {
    state[path] = isClosed(path) ? 'open' : 'closed';
    persist();
    apply();
  }

  for (const sec of sections) {
    const btn = sec.querySelector('.collapse-toggle');
    if (!btn) continue;
    btn.addEventListener('click', (e) => {
      e.preventDefault();
      toggle(sec.dataset.path);
    });
  }

  // ---- Tree -----------------------------------------------------------------

  function markActive(path) {
    for (const node of treeNodes) {
      node.classList.toggle('active', node.dataset.path === path);
    }
  }

  // Bring the active row into the sidebar's own scrollport without moving the
  // page. `scrollIntoView` would do both, which fights the scroll that
  // triggered this in the first place.
  function revealRow(node) {
    const scroller = sidebar && sidebar.querySelector('.tree-scroll');
    if (!scroller) return;
    const row = node.querySelector(':scope > .tree-row');
    if (!row) return;
    const rowBox = row.getBoundingClientRect();
    const box = scroller.getBoundingClientRect();
    if (rowBox.top < box.top) {
      scroller.scrollTop -= box.top - rowBox.top + 8;
    } else if (rowBox.bottom > box.bottom) {
      scroller.scrollTop += rowBox.bottom - box.bottom + 8;
    }
  }

  for (const node of treeNodes) {
    const twisty = node.querySelector(':scope > .tree-row > button.tree-twisty');
    if (twisty) {
      twisty.addEventListener('click', (e) => {
        e.preventDefault();
        toggle(node.dataset.path);
      });
    }

    const link = node.querySelector(':scope > .tree-row > .tree-link');
    if (!link) continue;
    link.addEventListener('click', (e) => {
      e.preventDefault();
      const path = node.dataset.path;
      // Open the folder and everything above it, or the section we are about
      // to scroll to would still be hidden by a collapsed ancestor.
      for (const p of paths) {
        if (p === path || isUnder(path, p)) state[p] = 'open';
      }
      persist();
      apply();
      markActive(path);
      const target = document.getElementById(node.dataset.target);
      if (target) target.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
  }

  for (const btn of document.querySelectorAll('[data-tree-all]')) {
    btn.addEventListener('click', () => {
      const open = btn.dataset.treeAll === 'open';
      for (const p of paths) state[p] = open ? 'open' : 'closed';
      // Collapsing *everything* would fold the root too and leave a page with
      // nothing on it. Keep the root open so the top level stays listed.
      if (!open && defaults.has('')) state[''] = 'open';
      persist();
      apply();
    });
  }

  // ---- Tree search ----------------------------------------------------------

  const search = sidebar && sidebar.querySelector('.tree-search-input');
  if (search) {
    const filter = () => {
      const q = search.value.trim().toLowerCase();
      sidebar.classList.toggle('searching', q !== '');
      if (!q) {
        for (const node of treeNodes) node.classList.remove('filtered-out');
        return;
      }
      const hits = new Set();
      for (const node of treeNodes) {
        if ((node.dataset.name || '').includes(q)) hits.add(node.dataset.path);
      }
      for (const node of treeNodes) {
        const path = node.dataset.path;
        let keep = hits.has(path);
        if (!keep) {
          // Keep the ancestors of a hit, so a match nested five levels down is
          // still reachable from the root.
          for (const hit of hits) {
            if (isUnder(hit, path)) { keep = true; break; }
          }
        }
        node.classList.toggle('filtered-out', !keep);
      }
    };
    search.addEventListener('input', filter);
    search.addEventListener('search', filter);
    filter();
  }

  // ---- Scroll spy -----------------------------------------------------------

  // Mirror the reading position in the tree. The band is deliberately narrow
  // (a slice near the top of the viewport) so exactly one section is "current"
  // even when several are on screen at once.
  if (treeNodes.length && sections.length && 'IntersectionObserver' in window) {
    const visible = new Set();
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) visible.add(entry.target);
        else visible.delete(entry.target);
      }
      for (const sec of sections) {
        if (!visible.has(sec)) continue;
        markActive(sec.dataset.path);
        const node = treeNodes.find((n) => n.dataset.path === sec.dataset.path);
        if (node) revealRow(node);
        break;
      }
    }, { rootMargin: '-8% 0px -85% 0px' });
    for (const sec of sections) observer.observe(sec);
  }

  apply();
})();
