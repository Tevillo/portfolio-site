(function () {
  // "Favorites only" toggle for /all: when on, hide every folder group whose
  // path isn't inside a directory named "favs". The choice is persisted so it
  // survives navigation, mirroring the collapse.js storage pattern.
  const STORAGE_KEY = 'portfolio-favs-only';
  const btn = document.querySelector('.favs-toggle');
  if (!btn) return;

  // A group qualifies if any path component is exactly "favs" (case-insensitive),
  // which covers the favs folder itself and anything nested beneath it.
  function isFav(path) {
    if (!path) return false;
    return path.split('/').some((seg) => seg.toLowerCase() === 'favs');
  }

  function apply(on) {
    document.querySelectorAll('section.gallery[data-path]').forEach((sec) => {
      sec.classList.toggle('favs-hidden', on && !isFav(sec.dataset.path));
    });
    btn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }

  let on = false;
  try { on = localStorage.getItem(STORAGE_KEY) === '1'; } catch (e) { /* ignore */ }

  btn.addEventListener('click', () => {
    on = !on;
    try { localStorage.setItem(STORAGE_KEY, on ? '1' : '0'); } catch (e) { /* ignore */ }
    apply(on);
  });

  apply(on);
})();
