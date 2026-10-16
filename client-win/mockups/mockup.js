// Review harness: state switcher (buttons, number keys, #hash) and fit-to-width scaling.
(function () {
  var buttons = Array.prototype.slice.call(document.querySelectorAll('.states button'));
  var caption = document.querySelector('.caption');
  function show(id) {
    var found = false;
    buttons.forEach(function (b) {
      var on = b.dataset.state === id;
      if (on) { found = true; if (caption) caption.innerHTML = b.dataset.caption || ''; }
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
    if (!found) return show(buttons[0].dataset.state);
    document.querySelectorAll('[data-view]').forEach(function (v) {
      v.hidden = v.dataset.view.split(' ').indexOf(id) < 0;
    });
    document.querySelectorAll('[data-only]').forEach(function (v) {
      v.hidden = v.dataset.only.split(' ').indexOf(id) < 0;
    });
    document.body.dataset.state = id;
    if (location.hash.slice(1) !== id) history.replaceState(null, '', '#' + id);
  }
  buttons.forEach(function (b, n) {
    b.insertAdjacentHTML('afterbegin', '<kbd>' + (n + 1) + '</kbd>');
    b.addEventListener('click', function () { show(b.dataset.state); });
  });
  document.addEventListener('keydown', function (e) {
    var n = parseInt(e.key, 10);
    if (n >= 1 && n <= buttons.length && !e.ctrlKey && !e.altKey && !e.metaKey) show(buttons[n - 1].dataset.state);
  });
  window.addEventListener('hashchange', function () { show(location.hash.slice(1)); });
  show(location.hash.slice(1) || buttons[0].dataset.state);

  var stage = document.querySelector('.stage');
  var inner = document.querySelector('.stage-inner');
  function fit() {
    var s = Math.min(1, stage.clientWidth / 1100);
    inner.style.transform = 'scale(' + s + ')';
    stage.style.height = Math.ceil(720 * s) + 'px';
  }
  window.addEventListener('resize', fit);
  fit();
})();
