(function () {
  // "Favorites only" toggle: when on, hide everything whose path isn't inside a
  // directory named "favs". On /all this hides whole folder groups; on a
  // person's page (a flat grid with no per-folder sections) it hides individual
  // tiles instead. The choice is persisted so it survives navigation, mirroring
  // the collapse.js storage pattern.
  const STORAGE_KEY = 'portfolio-favs-only';
  const btn = document.querySelector('.favs-toggle');
  if (!btn) return;

  // A path qualifies if any component is exactly "favs" (case-insensitive),
  // which covers the favs folder itself and anything nested beneath it.
  function isFav(path) {
    if (!path) return false;
    return path.split('/').some((seg) => seg.toLowerCase() === 'favs');
  }

  // A tile's path is recovered from its anchor href (`/image/<rel>`), which is
  // the only place the photo's folder is encoded on the flat grid.
  function tilePath(tile) {
    const a = tile.querySelector('a[href]');
    if (!a) return '';
    let p;
    try { p = new URL(a.href, location.origin).pathname; }
    catch (e) { p = a.getAttribute('href') || ''; }
    p = p.replace(/^\/(?:image|thumb|download)\//, '');
    try { return decodeURIComponent(p); } catch (e) { return p; }
  }

  function apply(on) {
    document.querySelectorAll('section.gallery[data-path]').forEach((sec) => {
      sec.classList.toggle('favs-hidden', on && !isFav(sec.dataset.path));
    });
    document.querySelectorAll('li.tile').forEach((tile) => {
      tile.classList.toggle('favs-hidden', on && !isFav(tilePath(tile)));
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
