// /notify — show only the handle field the chosen channel needs.
//
// Both fields are in the markup so the form still works with this file blocked
// or failing to load; all this does is hide the one that is not being used, so
// the page asks for one thing instead of two. The server reads whichever field
// matches the selected radio either way.
(function () {
  var radios = document.querySelectorAll('input[name="channel"]');
  var fields = document.querySelectorAll('.handle-field[data-channel]');
  if (!radios.length || !fields.length) return;

  function selected() {
    for (var i = 0; i < radios.length; i++) {
      if (radios[i].checked) return radios[i].value;
    }
    return null;
  }

  function sync() {
    var channel = selected();
    for (var i = 0; i < fields.length; i++) {
      var field = fields[i];
      var wanted = field.dataset.channel === channel;
      field.hidden = !wanted;
      // A hidden field must not block submission by staying required, and must
      // not carry a stale value into the POST.
      var input = field.querySelector('input');
      if (input) input.disabled = !wanted;
    }
  }

  for (var i = 0; i < radios.length; i++) {
    radios[i].addEventListener('change', sync);
  }
  sync();
})();
