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

  function persist() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
    } catch (e) { /* ignore */ }
  }

  function isClosed(path, defaultCollapsed) {
    const explicit = state[path];
    if (explicit === 'open') return false;
    if (explicit === 'closed') return true;
    return defaultCollapsed;
  }

  function isAncestorClosed(path) {
    for (const sec of document.querySelectorAll('section.gallery[data-path]')) {
      const p = sec.dataset.path;
      if (!p || p === path) continue;
      const isPrefix = p === '' || path.startsWith(p + '/');
      if (!isPrefix) continue;
      if (sec.classList.contains('collapsed')) return true;
    }
    return false;
  }

  function apply() {
    document.querySelectorAll('section.gallery[data-path]').forEach((sec) => {
      const path = sec.dataset.path;
      const defaultCollapsed = sec.dataset.defaultCollapsed === 'true';
      const closed = isClosed(path, defaultCollapsed);
      sec.classList.toggle('collapsed', closed);
      const btn = sec.querySelector('.collapse-toggle');
      if (btn) {
        btn.setAttribute('aria-expanded', closed ? 'false' : 'true');
        btn.setAttribute('aria-label', closed ? 'Expand folder' : 'Collapse folder');
      }
    });
    // Second pass: hide sections whose ancestor (by data-path prefix) is
    // collapsed. Only relevant on /all, where folder paths actually nest.
    document.querySelectorAll('section.gallery[data-path]').forEach((sec) => {
      sec.classList.toggle('hidden-by-ancestor', isAncestorClosed(sec.dataset.path));
    });
  }

  document.querySelectorAll('section.gallery[data-path] .collapse-toggle').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.preventDefault();
      const sec = btn.closest('section.gallery');
      const path = sec.dataset.path;
      const defaultCollapsed = sec.dataset.defaultCollapsed === 'true';
      const currentlyClosed = isClosed(path, defaultCollapsed);
      // Flip state, but write through to the explicit map so the choice
      // survives the next visit and overrides the default.
      state[path] = currentlyClosed ? 'open' : 'closed';
      persist();
      apply();
    });
  });

  apply();
})();
