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
  lb.innerHTML =
    '<button class="lb-btn lb-close" aria-label="Close">' + closeIcon + '</button>' +
    '<button class="lb-btn lb-nav lb-prev" aria-label="Previous">' + chevron('15,5 8,12 15,19') + '</button>' +
    '<div class="lb-stage"><img alt="" /></div>' +
    '<button class="lb-btn lb-nav lb-next" aria-label="Next">' + chevron('9,5 16,12 9,19') + '</button>';
  document.body.appendChild(lb);

  const imgEl = lb.querySelector('img');
  const closeBtn = lb.querySelector('.lb-close');
  const prevBtn = lb.querySelector('.lb-prev');
  const nextBtn = lb.querySelector('.lb-next');

  let urls = [];
  let idx = 0;

  function show() {
    imgEl.src = urls[idx];
  }
  function open(list, i) {
    urls = list;
    idx = i;
    show();
    lb.classList.add('open');
    lb.setAttribute('aria-hidden', 'false');
    document.body.style.overflow = 'hidden';
  }
  function close() {
    lb.classList.remove('open');
    lb.setAttribute('aria-hidden', 'true');
    document.body.style.overflow = '';
    imgEl.src = '';
  }
  function next() {
    if (!urls.length) return;
    idx = (idx + 1) % urls.length;
    show();
  }
  function prev() {
    if (!urls.length) return;
    idx = (idx - 1 + urls.length) % urls.length;
    show();
  }

  document.querySelectorAll('ul.grid').forEach((grid) => {
    const links = Array.from(grid.querySelectorAll('li.tile a'));
    const list = links.map((a) => a.href);
    links.forEach((a, i) => {
      a.addEventListener('click', (e) => {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button === 1) return;
        e.preventDefault();
        open(list, i);
      });
    });
  });

  closeBtn.addEventListener('click', close);
  prevBtn.addEventListener('click', (e) => { e.stopPropagation(); prev(); });
  nextBtn.addEventListener('click', (e) => { e.stopPropagation(); next(); });
  lb.addEventListener('click', (e) => {
    if (e.target.closest('.lb-btn')) return;
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
    if (e.key === 'Escape') close();
    else if (e.key === 'ArrowLeft') prev();
    else if (e.key === 'ArrowRight') next();
  });
})();
