(function () {
  const STORAGE_KEY = 'portfolio-collapsed-folders';
  let collapsed = new Set();
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) collapsed = new Set(JSON.parse(raw));
  } catch (e) { /* ignore */ }

  function persist() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify([...collapsed]));
    } catch (e) { /* ignore */ }
  }

  function isAncestorCollapsed(path) {
    for (const p of collapsed) {
      if (p === path) continue;
      if (p === '') return true;
      if (path.startsWith(p + '/')) return true;
    }
    return false;
  }

  function apply() {
    document.querySelectorAll('section.gallery[data-path]').forEach((sec) => {
      const path = sec.dataset.path;
      const selfCollapsed = collapsed.has(path);
      sec.classList.toggle('collapsed', selfCollapsed);
      sec.classList.toggle('hidden-by-ancestor', isAncestorCollapsed(path));
      const btn = sec.querySelector('.collapse-toggle');
      if (btn) {
        btn.setAttribute('aria-expanded', selfCollapsed ? 'false' : 'true');
        btn.setAttribute('aria-label', selfCollapsed ? 'Expand folder' : 'Collapse folder');
      }
    });
  }

  document.querySelectorAll('section.gallery[data-path] .collapse-toggle').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.preventDefault();
      const sec = btn.closest('section.gallery');
      const path = sec.dataset.path;
      if (collapsed.has(path)) collapsed.delete(path);
      else collapsed.add(path);
      persist();
      apply();
    });
  });

  apply();
})();
