(function () {
  const lb = document.createElement('div');
  lb.className = 'lightbox';
  lb.setAttribute('aria-hidden', 'true');
  const chevron = (points) =>
    '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">' +
    '<polyline points="' + points + '" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/>' +
    '</svg>';
  const closeIcon =
    '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">' +
    '<line x1="6" y1="6" x2="18" y2="18" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>' +
    '<line x1="18" y1="6" x2="6" y2="18" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>' +
    '</svg>';
  const downloadIcon =
    '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">' +
    '<path d="M12 4v9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>' +
    '<polyline points="8,10 12,14 16,10" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/>' +
    '<path d="M5 19h14" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>' +
    '</svg>';
  lb.innerHTML =
    '<button class="lb-btn lb-close" aria-label="Close">' + closeIcon + '</button>' +
    '<div class="lb-dl" hidden>' +
      '<button type="button" class="lb-btn lb-dl-toggle" aria-label="Download" aria-haspopup="true" aria-expanded="false">' + downloadIcon + '</button>' +
      '<div class="lb-dl-menu" hidden>' +
        '<a class="lb-dl-option" data-kind="jpg" download>JPG</a>' +
        '<a class="lb-dl-option" data-kind="raw" download hidden>RAW</a>' +
      '</div>' +
    '</div>' +
    '<button class="lb-btn lb-nav lb-prev" aria-label="Previous">' + chevron('15,5 8,12 15,19') + '</button>' +
    '<div class="lb-stage">' +
      '<img alt="" />' +
      '<div class="lb-caption" hidden>' +
        '<span class="lb-name"></span>' +
        '<button type="button" class="lb-download" hidden>Download</button>' +
      '</div>' +
    '</div>' +
    '<button class="lb-btn lb-nav lb-next" aria-label="Next">' + chevron('9,5 16,12 9,19') + '</button>';
  document.body.appendChild(lb);

  const imgEl = lb.querySelector('img');
  const closeBtn = lb.querySelector('.lb-close');
  const prevBtn = lb.querySelector('.lb-prev');
  const nextBtn = lb.querySelector('.lb-next');
  const captionEl = lb.querySelector('.lb-caption');
  const nameEl = lb.querySelector('.lb-name');
  const downloadBtn = lb.querySelector('.lb-download');
  const dlWrap = lb.querySelector('.lb-dl');
  const dlToggle = lb.querySelector('.lb-dl-toggle');
  const dlMenu = lb.querySelector('.lb-dl-menu');
  const dlJpg = lb.querySelector('.lb-dl-option[data-kind="jpg"]');
  const dlRaw = lb.querySelector('.lb-dl-option[data-kind="raw"]');

  function closeDlMenu() {
    dlMenu.hidden = true;
    dlToggle.setAttribute('aria-expanded', 'false');
  }

  // Each item: { url, preview?, name?, download?, jpg?, raw? }. `preview` is the
  // already-loaded thumbnail URL; we paint that immediately on click so the
  // lightbox feels instant, then upgrade to the full-size `url` when it
  // finishes downloading in the background.
  let items = [];
  let idx = 0;

  // Track which full-size URLs we've already kicked off so we don't waste
  // bandwidth issuing duplicate fetches across neighbor-prefetch +
  // background-prefetch + Image() preloaders.
  const prefetched = new Set();

  function prefetch(url) {
    if (!url || prefetched.has(url)) return;
    prefetched.add(url);
    const img = new Image();
    img.decoding = 'async';
    img.src = url;
    return img;
  }

  function show() {
    const it = items[idx] || {};
    const hasName = !!it.name;
    const hasDownload = !!it.download;
    nameEl.textContent = hasName ? it.name : '';
    downloadBtn.hidden = !hasDownload;
    downloadBtn.dataset.action = hasDownload ? it.download : '';
    captionEl.hidden = !(hasName || hasDownload);

    // Side download menu (regular galleries): show when this item carries a
    // JPG download URL; reveal the RAW option only when a sibling raw exists.
    // Always collapse the menu when the shown item changes.
    closeDlMenu();
    const hasJpg = !!it.jpg;
    dlWrap.hidden = !hasJpg;
    if (hasJpg) {
      dlJpg.href = it.jpg;
      if (it.raw) {
        dlRaw.href = it.raw;
        dlRaw.hidden = false;
      } else {
        dlRaw.removeAttribute('href');
        dlRaw.hidden = true;
      }
    }

    // Instant feedback: paint the low-res preview right away. The browser
    // already has it in cache from rendering the tile.
    const placeholder = it.preview || it.url || '';
    imgEl.src = placeholder;

    // Then load the full-size in the background; swap in once it's ready,
    // but only if the user hasn't already moved to a different item.
    if (it.url && it.url !== it.preview) {
      const targetItem = it;
      const loader = new Image();
      loader.decoding = 'async';
      loader.onload = () => {
        if (items[idx] === targetItem) {
          imgEl.src = it.url;
        }
      };
      loader.src = it.url;
      prefetched.add(it.url);
    }

    // Warm the immediate neighbors so prev/next clicks are snappy.
    if (items.length > 1) {
      prefetch(items[(idx + 1) % items.length] && items[(idx + 1) % items.length].url);
      prefetch(items[(idx - 1 + items.length) % items.length] && items[(idx - 1 + items.length) % items.length].url);
    }
  }
  function open(list, i) {
    items = list;
    idx = i;
    show();
    lb.classList.add('open');
    lb.setAttribute('aria-hidden', 'false');
    document.body.style.overflow = 'hidden';
  }
  function close() {
    closeDlMenu();
    lb.classList.remove('open');
    lb.setAttribute('aria-hidden', 'true');
    document.body.style.overflow = '';
    imgEl.src = '';
  }
  function next() {
    if (!items.length) return;
    idx = (idx + 1) % items.length;
    show();
  }
  function prev() {
    if (!items.length) return;
    idx = (idx - 1 + items.length) % items.length;
    show();
  }

  // Each (groupSelector, linkSelector) pair renders an independent lightbox
  // sequence, so prev/next wraps within one gallery / work section.
  //
  // `.mgrid` is the portfolio's column grid. It uses its own tile class rather
  // than `li.tile` because it is laid out in flex columns rather than in the
  // square-crop grid, and the group selector is the outermost container rather
  // than a column or a band, so prev/next walks the whole section instead of
  // stopping at the bottom of one column or at the next full-width panorama.
  //
  // Its anchors carry the same `data-name` / `data-jpg` / `data-raw` as any
  // other tile, plus `data-seq`. That last one matters here: the portfolio's
  // markup is grouped by column, so document order is column-major (1, 4, 7,
  // 2, 5, 8, ...) while the page *reads* 1, 2, 3. `data-seq` is the reading
  // index, and the sort below is what stops the arrow keys walking down one
  // column and back up the next.
  const groups = [
    ['ul.grid', 'li.tile a'],
    ['.mgrid', 'li.mtile a'],
  ];
  groups.forEach(([groupSel, linkSel]) => {
    document.querySelectorAll(groupSel).forEach((group) => {
      const links = Array.from(group.querySelectorAll(linkSel));
      // Walk the sequence the way the page reads, when it says so. Absent
      // `data-seq` this is a no-op and document order stands, which is what
      // every other grid on the site wants.
      if (links.length && links[0].dataset.seq !== undefined) {
        links.sort((a, b) => Number(a.dataset.seq) - Number(b.dataset.seq));
      }
      const list = links.map((a) => {
        const innerImg = a.querySelector('img');
        return {
          url: a.href,
          preview: innerImg ? innerImg.src : '',
          name: a.dataset.name || '',
          download: a.dataset.download || '',
          jpg: a.dataset.jpg || '',
          raw: a.dataset.raw || '',
        };
      });
      links.forEach((a, i) => {
        a.addEventListener('click', (e) => {
          if (e.metaKey || e.ctrlKey || e.shiftKey || e.button === 1) return;
          e.preventDefault();
          open(list, i);
        });
      });
    });
  });

  closeBtn.addEventListener('click', close);
  prevBtn.addEventListener('click', (e) => { e.stopPropagation(); prev(); });
  nextBtn.addEventListener('click', (e) => { e.stopPropagation(); next(); });
  // Submit a one-shot form. Auth rides on the path-scoped work cookie set
  // when the password was accepted, so the browser handles streaming the
  // response as an attachment without any extra fields here.
  downloadBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const action = downloadBtn.dataset.action;
    if (!action) return;
    const form = document.createElement('form');
    form.method = 'post';
    form.action = action;
    form.style.display = 'none';
    document.body.appendChild(form);
    form.submit();
    form.remove();
  });
  // Toggle the JPG/RAW menu without closing the lightbox.
  dlToggle.addEventListener('click', (e) => {
    e.stopPropagation();
    const willOpen = dlMenu.hidden;
    dlMenu.hidden = !willOpen;
    dlToggle.setAttribute('aria-expanded', willOpen ? 'true' : 'false');
  });
  // The JPG/RAW links are plain GET downloads (the `/download` endpoint sends
  // an attachment Content-Disposition); just collapse the menu after a pick.
  dlMenu.addEventListener('click', (e) => {
    e.stopPropagation();
    if (e.target.closest('.lb-dl-option')) closeDlMenu();
  });
  lb.addEventListener('click', (e) => {
    if (e.target.closest('.lb-btn') || e.target.closest('.lb-caption') || e.target.closest('.lb-dl')) return;
    // A backdrop click closes the open menu first, the lightbox second.
    if (!dlMenu.hidden) { closeDlMenu(); return; }
    close();
  });

  let touchX = 0, touchY = 0, touchT = 0;
  lb.addEventListener('touchstart', (e) => {
    if (e.touches.length !== 1) return;
    touchX = e.touches[0].clientX;
    touchY = e.touches[0].clientY;
    touchT = Date.now();
  }, { passive: true });
  lb.addEventListener('touchend', (e) => {
    if (e.changedTouches.length !== 1) return;
    const dx = e.changedTouches[0].clientX - touchX;
    const dy = e.changedTouches[0].clientY - touchY;
    const dt = Date.now() - touchT;
    if (dt < 600 && Math.abs(dx) > 40 && Math.abs(dx) > Math.abs(dy) * 1.5) {
      if (dx < 0) next(); else prev();
    }
  });

  document.addEventListener('keydown', (e) => {
    if (!lb.classList.contains('open')) return;
    if (e.key === 'Escape') { if (!dlMenu.hidden) closeDlMenu(); else close(); }
    else if (e.key === 'ArrowLeft') prev();
    else if (e.key === 'ArrowRight') next();
  });

  // Work-page-only behavior: background prefetch of full-size images.
  // Gated on `main.work` so /all and /browse don't pay for it. (The JPEG,
  // RAW and Both download buttons are plain form submits — no JS needed.)
  if (document.querySelector('main.work')) {
    // Background prefetch: drain a low-concurrency queue of every
    const tiles = Array.from(
      document.querySelectorAll(
        'main.work section.gallery:not(.collapsed) ul.grid li.tile a'
      )
    );
    if (tiles.length) {
      // Skim off the first few synchronously after a brief settle so the
      // visible viewport gets head-of-queue treatment; the rest pump via
      // requestIdleCallback so we don't fight the user's interactions.
      let i = 0;
      function pump() {
        if (i >= tiles.length) return;
        const a = tiles[i++];
        prefetch(a.href);
        if ('requestIdleCallback' in window) {
          requestIdleCallback(pump, { timeout: 500 });
        } else {
          setTimeout(pump, 80);
        }
      }
      // Wait ~1s so the in-viewport thumbnails finish loading first, then
      // start two parallel drain loops (browser further throttles per-host).
      setTimeout(() => { pump(); pump(); }, 1000);
    }
  }
})();
