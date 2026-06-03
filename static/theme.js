(function () {
  var KEY = 'portfolio-theme';
  var btn = document.querySelector('.theme-toggle');
  if (!btn) return;

  function current() {
    return document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light';
  }
  function apply(theme) {
    document.documentElement.dataset.theme = theme;
    btn.setAttribute('aria-pressed', theme === 'dark');
    btn.setAttribute(
      'aria-label',
      theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'
    );
  }
  apply(current());

  btn.addEventListener('click', function () {
    var next = current() === 'dark' ? 'light' : 'dark';
    apply(next);
    try {
      localStorage.setItem(KEY, next);
    } catch (e) {
      /* localStorage unavailable; toggle still works for this session */
    }
  });
})();
