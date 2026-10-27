(function () {
  var D = JSON.parse(document.getElementById('data').textContent);
  var ORDER = D.order, DOM = D.domains, HOSTS = Object.keys(DOM), PUBLIC = !!D.public;
  if (PUBLIC) document.getElementById('q').placeholder = 'domain';
  var CAP = 14;
  var state = { q: '', min: 1, sort: 'n', open: {} };
  try { var s = localStorage.getItem('rd.state'); if (s) { s = JSON.parse(s); state.min = s.min || 1; state.sort = s.sort || 'n'; } } catch (e) {}

  var board = document.getElementById('board'), toc = document.getElementById('toc'), top = document.getElementById('top');
  var totalEntries = 0, firstDate = '9999', lastDate = '0000';
  HOSTS.forEach(function (h) { var d = DOM[h]; totalEntries += d.n; if (d.first < firstDate) firstDate = d.first; if (d.last > lastDate) lastDate = d.last; });
  document.getElementById('stats').innerHTML = '<b>' + HOSTS.length.toLocaleString() + '</b> domains &middot; <b>' + totalEntries.toLocaleString() + '</b> saved pages &middot; ' + firstDate.slice(0, 7) + ' to ' + lastDate.slice(0, 7);

  function esc(s) { return s.replace(/[&<>"]/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]; }); }
  function slug(s) { return s.replace(/[^a-z0-9]+/gi, '-').replace(/^-|-$/g, '').toLowerCase(); }
  function hi(text, q) {
    if (!q) return esc(text);
    var i = text.toLowerCase().indexOf(q);
    if (i < 0) return esc(text);
    return esc(text.slice(0, i)) + '<mark>' + esc(text.slice(i, i + q.length)) + '</mark>' + esc(text.slice(i + q.length));
  }
  function matches(h, q) {
    if (!q) return true;
    if (h.indexOf(q) >= 0) return true;
    if (PUBLIC) return false;
    var es = DOM[h].e;
    for (var i = 0; i < es.length; i++) if (es[i][0].toLowerCase().indexOf(q) >= 0) return true;
    return false;
  }
  function cmp(a, b) {
    if (state.sort === 'n' && DOM[a].n !== DOM[b].n) return DOM[b].n - DOM[a].n;
    return a < b ? -1 : a > b ? 1 : 0;
  }

  function pagesHtml(h) {
    var d = DOM[h], q = state.q;
    var out = '<ul class="pages">';
    out += '<li class="meta">' + d.n + (d.n === 1 ? ' page' : ' pages') + ', ' + d.first + ' to ' + d.last + '</li>';
    for (var i = d.e.length - 1; i >= 0; i--) {
      var e = d.e[i];
      out += '<li><a href="' + esc(e[1]) + '" target="_blank" rel="noopener">' + hi(e[0] || e[1], q) + '</a><time>' + e[2] + '</time></li>';
    }
    return out + '</ul>';
  }

  function render() {
    var q = state.q, visible = {}, topicTotals = [];
    var html = '', any = false;
    ORDER.forEach(function (t) {
      var topicHtml = '', topicDomains = 0, topicPages = 0;
      t.subs.forEach(function (s) {
        var hs = s.hosts.filter(function (h) { return DOM[h].n >= state.min && matches(h, q); }).sort(cmp);
        if (!hs.length) return;
        topicDomains += hs.length;
        var key = slug(t.name + ' ' + s.name), showAll = q || state.open['sub:' + key];
        var lim = showAll ? hs.length : Math.min(CAP, hs.length);
        var li = '';
        for (var i = 0; i < lim; i++) {
          var h = hs[i]; topicPages += DOM[h].n;
          var isOpen = !!state.open[h];
          li += '<li class="d"><a class="h" href="https://' + esc(h) + '" target="_blank" rel="noopener"' + (PUBLIC ? '' : ' title="' + esc(DOM[h].e[0][0]) + '"') + '>' + hi(h, q) + '</a>' +
                (PUBLIC ? '<span class="n" title="' + DOM[h].n + ' saved">' + DOM[h].n + '</span>'
                        : '<button type="button" class="n" data-h="' + esc(h) + '" aria-expanded="' + isOpen + '" title="show saved pages">' + DOM[h].n + '</button>') + '</li>';
          if (isOpen && !PUBLIC) li += '<li>' + pagesHtml(h) + '</li>';
        }
        for (var j = lim; j < hs.length; j++) topicPages += DOM[hs[j]].n;
        if (hs.length > lim) li += '<li class="more"><button type="button" data-sub="' + key + '">+ ' + (hs.length - lim) + ' more</button></li>';
        else if (!q && showAll && hs.length > CAP) li += '<li class="more"><button type="button" data-sub="' + key + '">show fewer</button></li>';
        topicHtml += '<section class="sub"><h3>' + esc(s.name) + '<span class="n">' + hs.length + '</span></h3><ul>' + li + '</ul></section>';
      });
      if (!topicDomains) return;
      any = true;
      topicTotals.push([t.name, topicDomains]);
      html += '<section class="topic" id="' + slug(t.name) + '"><h2>' + esc(t.name) + '<span class="n">' + topicDomains + ' domains &middot; ' + topicPages + ' pages</span></h2>' + topicHtml + '</section>';
    });
    board.innerHTML = any ? html : '<p class="empty">Nothing matches "' + esc(q) + '" at this filter. Try fewer letters or set min saves to all.</p>';
    toc.innerHTML = topicTotals.map(function (p) { return '<li><a href="#' + slug(p[0]) + '">' + esc(p[0]) + '</a><span class="n">' + p[1] + '</span></li>'; }).join('');
  }

  var topHosts = HOSTS.slice().sort(function (a, b) { return DOM[b].n - DOM[a].n || (a < b ? -1 : 1); }).slice(0, 15);
  top.innerHTML = topHosts.map(function (h) { return '<li><a href="https://' + esc(h) + '" target="_blank" rel="noopener">' + esc(h) + '</a><span class="n">' + DOM[h].n + '</span></li>'; }).join('');

  function save() { try { localStorage.setItem('rd.state', JSON.stringify({ min: state.min, sort: state.sort })); } catch (e) {} }
  function setSeg(id, attr, val) {
    document.getElementById(id).querySelectorAll('button').forEach(function (b) { b.setAttribute('aria-pressed', String(b.dataset[attr] === String(val))); });
  }
  var q = document.getElementById('q'), timer;
  q.addEventListener('input', function () { clearTimeout(timer); timer = setTimeout(function () { state.q = q.value.trim().toLowerCase(); render(); }, 120); });
  document.getElementById('minseg').addEventListener('click', function (ev) { var b = ev.target.closest('button'); if (!b) return; state.min = +b.dataset.min; setSeg('minseg', 'min', state.min); save(); render(); });
  document.getElementById('sortseg').addEventListener('click', function (ev) { var b = ev.target.closest('button'); if (!b) return; state.sort = b.dataset.sort; setSeg('sortseg', 'sort', state.sort); save(); render(); });
  board.addEventListener('click', function (ev) {
    var b = ev.target.closest('button'); if (!b) return;
    if (b.dataset.h) { state.open[b.dataset.h] = !state.open[b.dataset.h]; render(); }
    else if (b.dataset.sub) { state.open['sub:' + b.dataset.sub] = !state.open['sub:' + b.dataset.sub]; render(); }
  });
  setSeg('minseg', 'min', state.min); setSeg('sortseg', 'sort', state.sort);
  render();
  if (location.hash) { var el = document.getElementById(location.hash.slice(1)); if (el) el.scrollIntoView(); }
})();
