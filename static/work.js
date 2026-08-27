// Reopen the download panel after a failed password.
//
// The panel is a popover, so it is closed on every page load — including the
// one the server renders after rejecting a password. That page would otherwise
// say "Incorrect password." with no field anywhere near it, and the client
// would have to work out that the answer is behind the Download button again.
//
// Everything else about the panel is declarative: `popovertarget` opens and
// closes it, and the browser supplies the backdrop, Escape and light-dismiss.
// This file exists for the one state that cannot wait for a click.
(function () {
  var pop = document.getElementById('work-downloads');
  // `data-auto-open` is set by the view only on the error re-render, and
  // `showPopover` is missing on a browser too old for the Popover API — where
  // the panel is rendered inline and already visible, so there is nothing to
  // open.
  if (!pop || !pop.dataset.autoOpen || typeof pop.showPopover !== 'function') return;
  try {
    pop.showPopover();
  } catch (e) {
    // Already open, or the element is not in the document yet. Either way the
    // banner on the page has said what happened.
  }
})();
