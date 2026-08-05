// Live-DOM cosmetic filtering.
//
// The proxy scans a page for class and id names once, as it is served, and
// ships the generic rules that match. A page that builds itself in JavaScript
// has almost nothing to scan at that moment. This watches for elements the
// page adds afterwards, reports the names it has not asked about yet, and
// applies whatever CSS comes back.
//
// The endpoint is on the page's own origin: the request goes back through the
// proxy like any other, and Adblock recognises the path and answers it itself.
// Same origin means no CORS and no mixed content, and it works from whichever
// machine is browsing. __ROUTE_PREFIX__ is replaced with the path Adblock
// reserves before this is injected.
(function () {
  var endpoint = location.origin + "__ROUTE_PREFIX__cosmetic";
  var BATCH_MS = 100;
  var MAX_NAMES = 500; // per request
  var MAX_ROUNDS = 20; // a page that rewrites itself forever stops here

  var asked = Object.create(null); // every name already sent
  var queue = [];
  var rounds = 0;
  var timer = null;
  var sheet = null;

  function apply(css) {
    if (!css) return;
    if (!sheet) {
      sheet = document.createElement("style");
      sheet.setAttribute("type", "text/css");
      (document.head || document.documentElement).appendChild(sheet);
    }
    sheet.appendChild(document.createTextNode(css));
  }

  function flush() {
    timer = null;
    if (!queue.length || rounds >= MAX_ROUNDS) return;
    rounds++;

    var batch = queue.splice(0, MAX_NAMES);
    var classes = [];
    var ids = [];
    for (var i = 0; i < batch.length; i++) {
      var name = batch[i];
      (name.charCodeAt(0) === 35 ? ids : classes).push(name.slice(1));
    }

    // text/plain keeps this a "simple" cross-origin request, so the browser
    // sends it without a preflight.
    fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "text/plain" },
      body: JSON.stringify({ url: location.href, classes: classes, ids: ids }),
      credentials: "omit",
      cache: "no-store",
    })
      .then(function (r) {
        return r.json();
      })
      .then(function (d) {
        apply(d && d.css);
      })
      .catch(function () {});

    if (queue.length) schedule();
  }

  function schedule() {
    if (!timer) timer = setTimeout(flush, BATCH_MS);
  }

  function note(name) {
    if (asked[name]) return;
    asked[name] = true;
    queue.push(name);
    schedule();
  }

  function scan(node) {
    if (!node || node.nodeType !== 1) return;
    if (node.id) note("#" + node.id);
    var list = node.classList;
    if (list) {
      for (var i = 0; i < list.length; i++) note("." + list[i]);
    }
    var kids = node.children;
    if (kids) {
      for (var j = 0; j < kids.length; j++) scan(kids[j]);
    }
  }

  new MutationObserver(function (records) {
    if (rounds >= MAX_ROUNDS) return;
    for (var i = 0; i < records.length; i++) {
      var r = records[i];
      if (r.type === "attributes") {
        scan(r.target);
        continue;
      }
      for (var j = 0; j < r.addedNodes.length; j++) scan(r.addedNodes[j]);
    }
  }).observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["class", "id"],
  });
})();
