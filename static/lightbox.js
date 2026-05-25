(function () {
  const lb = document.createElement('div');
  lb.className = 'lightbox';
  lb.setAttribute('aria-hidden', 'true');
  lb.innerHTML =
    '<button class="lb-btn lb-close" aria-label="Close">&times;</button>' +
    '<button class="lb-btn lb-nav lb-prev" aria-label="Previous">&#x2039;</button>' +
    '<div class="lb-stage"><img alt="" /></div>' +
    '<button class="lb-btn lb-nav lb-next" aria-label="Next">&#x203A;</button>';
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
  lb.addEventListener('click', (e) => { if (e.target === lb || e.target.classList.contains('lb-stage')) close(); });

  document.addEventListener('keydown', (e) => {
    if (!lb.classList.contains('open')) return;
    if (e.key === 'Escape') close();
    else if (e.key === 'ArrowLeft') prev();
    else if (e.key === 'ArrowRight') next();
  });
})();
