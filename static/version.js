(function () {
  // Reload a page that is running an outdated build. The build id is rendered
  // into every page as <meta name="build-version"> and served live at
  // /version; when they disagree, this tab is stale.
  //
  // The check is event-driven only — no polling — so an idle tab costs nothing.
  // The trade-off is that a tab that never loses focus never notices; the
  // reload happens the next time the user comes back to it.
  var meta = document.querySelector('meta[name="build-version"]');
  if (!meta || !window.fetch) return;

  var CURRENT = meta.getAttribute('content') || '';
  if (!CURRENT) return;

  // Build ids are lowercase hex (see views::build_id). Anything else is not a
  // version — most importantly the proxy's own error page while the service is
  // restarting, which would otherwise read as "a new build" and reload forever
  // against a server that is still down.
  var VERSION_RE = /^[0-9a-f]{1,16}$/;

  // If a proxy ever serves cached HTML, a reload would return the same old
  // meta while /version (no-store) keeps reporting the new build — an endless
  // loop. Remember which build we already reloaded *towards*: if we land back
  // here still stale, we stop rather than trying again.
  var ATTEMPT_KEY = 'portfolio-reload-attempt';
  var MIN_INTERVAL_MS = 60000;
  var checking = false;

  function readAttempt() {
    try {
      return JSON.parse(sessionStorage.getItem(ATTEMPT_KEY) || 'null');
    } catch (e) {
      return null;
    }
  }

  function alreadyTried(target) {
    var prev = readAttempt();
    if (!prev) return false;
    if (prev.target === target) return true;
    return Date.now() - prev.at < MIN_INTERVAL_MS;
  }

  function recordAttempt(target) {
    try {
      sessionStorage.setItem(
        ATTEMPT_KEY,
        JSON.stringify({ target: target, at: Date.now() })
      );
    } catch (e) {
      /* private mode; the other guards still apply */
    }
  }

  // Don't yank the page out from under someone mid-task.
  function busy() {
    // Lightbox open — reloading would close the photo they're looking at.
    if (document.querySelector('.lightbox.open')) return true;

    // The work password page is rendered directly from a POST, so reload()
    // would trigger the browser's "Confirm Form Resubmission" prompt.
    if (document.querySelector('#work-password')) return true;

    // Focused text entry.
    var el = document.activeElement;
    if (el && /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName)) return true;

    // Typed something and then clicked away — activeElement is <body> by then,
    // so the focus check above misses it.
    var fields = document.querySelectorAll(
      'input[type="password"], input[type="text"], input[type="search"], textarea'
    );
    for (var i = 0; i < fields.length; i++) {
      if (fields[i].value) return true;
    }

    // The graph view keeps pan/zoom in memory only; a reload resets the view.
    if (document.getElementById('graph-canvas')) return true;

    return false;
  }

  function check() {
    if (checking || document.visibilityState !== 'visible') return;
    checking = true;
    fetch('/version', { cache: 'no-store', credentials: 'same-origin' })
      .then(function (res) {
        if (!res.ok) throw new Error('bad status');
        return res.text();
      })
      .then(function (text) {
        var latest = text.trim();
        if (!VERSION_RE.test(latest)) return;
        if (latest === CURRENT) return;
        if (alreadyTried(latest)) return;
        if (busy()) return;
        recordAttempt(latest);
        location.reload();
      })
      .catch(function () {
        /* offline, or the server is mid-restart; try again on the next event */
      })
      .then(function () {
        checking = false;
      });
  }

  document.addEventListener('visibilitychange', check);
  window.addEventListener('online', check);
  window.addEventListener('pageshow', function (e) {
    // Only when restored from bfcache. On a normal load the meta value came
    // from the very process we would be asking, so the check is guaranteed to
    // match and would just be a wasted request on every page view.
    if (e.persisted) check();
  });
})();
