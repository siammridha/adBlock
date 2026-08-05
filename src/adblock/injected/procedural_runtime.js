// Procedural cosmetic filtering.
//
// The cosmetic rules a stylesheet cannot express: pick elements by the text
// inside them, by an ancestor, by computed style, by an XPath expression — and
// act on them by deleting the node, an attribute or a class, not only by hiding
// it. CSS can make something invisible; it cannot take it out of the document,
// and some anti-adblock code checks for presence rather than visibility.
//
// The proxy embeds this page's own rules below, so unlike the class/id lookup
// in cosmetic_runtime.js this asks the proxy nothing and works in every
// browser. __PROCEDURAL_FILTERS__ is replaced with the JSON the filter engine
// produced for this page before this is injected.
(function () {
  var FILTERS = __PROCEDURAL_FILTERS__;
  var DEBOUNCE_MS = 50;

  // The engine only ever puts a plain CSS selector first in a chain. Anything
  // else is a shape we do not know how to walk, and matching nothing is the
  // safe reading of a rule we cannot read.
  FILTERS = FILTERS.filter(function (f) {
    var ops = f.selector || [];
    return (
      ops.length > 0 &&
      ops.every(function (op, i) {
        return i === 0 || op.type !== "css-selector";
      })
    );
  });
  if (FILTERS.length === 0) return;

  function escapeRe(s) {
    return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  }

  // "/pattern/flags" is a regular expression; anything else is literal.
  function toRegex(s) {
    var m = /^\/(.+)\/([a-z]*)$/.exec(s);
    if (!m) return null;
    try {
      return new RegExp(m[1], m[2]);
    } catch (e) {
      return null;
    }
  }

  // :has-text() and :matches-path() — a substring, or a regex.
  function contains(arg) {
    var re = toRegex(arg);
    if (re) {
      return function (v) {
        return re.test(v);
      };
    }
    return function (v) {
      return v.indexOf(arg) !== -1;
    };
  }

  // :matches-attr() and :matches-css() values — exact, `*` wildcards, or a regex.
  function equals(arg) {
    var re = toRegex(arg);
    if (re) {
      return function (v) {
        return re.test(v);
      };
    }
    var s = arg.replace(/^["']|["']$/g, "");
    if (s.indexOf("*") === -1) {
      return function (v) {
        return v === s;
      };
    }
    var rx = new RegExp("^" + s.split("*").map(escapeRe).join(".*") + "$");
    return function (v) {
      return rx.test(v);
    };
  }

  function qsa(selector) {
    try {
      return Array.prototype.slice.call(document.querySelectorAll(selector));
    } catch (e) {
      return [];
    }
  }

  function attrTest(arg) {
    var eq = arg.indexOf("=");
    var nameMatch = equals(eq === -1 ? arg : arg.slice(0, eq));
    var valueMatch = eq === -1 ? null : equals(arg.slice(eq + 1));
    return function (el) {
      var attrs = el.attributes;
      for (var i = 0; i < attrs.length; i++) {
        if (!nameMatch(attrs[i].name)) continue;
        if (!valueMatch || valueMatch(attrs[i].value)) return true;
      }
      return false;
    };
  }

  function cssTest(arg, pseudo) {
    var colon = arg.indexOf(":");
    if (colon === -1) {
      return function () {
        return false;
      };
    }
    var prop = arg.slice(0, colon).trim();
    var valueMatch = equals(arg.slice(colon + 1).trim());
    return function (el) {
      var style = getComputedStyle(el, pseudo);
      return !!style && valueMatch(style.getPropertyValue(prop));
    };
  }

  // :upward(3) walks up that many parents; :upward(selector) finds the nearest
  // ancestor that matches.
  function upward(el, arg) {
    var n = parseInt(arg, 10);
    if (String(n) === arg.trim()) {
      var up = el;
      while (n-- > 0 && up) up = up.parentElement;
      return up;
    }
    try {
      return el.parentElement ? el.parentElement.closest(arg) : null;
    } catch (e) {
      return null;
    }
  }

  function xpath(el, expr) {
    var out = [];
    try {
      // 7 is ORDERED_NODE_SNAPSHOT_TYPE: a static list, safe to act on while
      // the document changes under it.
      var r = document.evaluate(expr, el, null, 7, null);
      for (var i = 0; i < r.snapshotLength; i++) {
        var n = r.snapshotItem(i);
        if (n.nodeType === 1) out.push(n);
      }
    } catch (e) {}
    return out;
  }

  // Walk each element somewhere else, keeping the result a set.
  function walk(nodes, f) {
    var out = [];
    for (var i = 0; i < nodes.length; i++) {
      var got = f(nodes[i]);
      if (!got) continue;
      if (got.nodeType) got = [got];
      for (var j = 0; j < got.length; j++) {
        if (out.indexOf(got[j]) === -1) out.push(got[j]);
      }
    }
    return out;
  }

  // One operator: narrow the current set, or move it.
  function step(nodes, op) {
    var arg = op.arg;
    switch (op.type) {
      case "has-text":
        var text = contains(arg);
        return nodes.filter(function (el) {
          return text(el.textContent);
        });
      case "min-text-length":
        var min = parseInt(arg, 10);
        return nodes.filter(function (el) {
          return el.textContent.length >= min;
        });
      case "matches-path":
        // A page-level test: either every element stays or none do.
        return contains(arg)(location.pathname + location.search) ? nodes : [];
      case "matches-attr":
        return nodes.filter(attrTest(arg));
      case "matches-css":
        return nodes.filter(cssTest(arg, null));
      case "matches-css-before":
        return nodes.filter(cssTest(arg, ":before"));
      case "matches-css-after":
        return nodes.filter(cssTest(arg, ":after"));
      case "upward":
        return walk(nodes, function (el) {
          return upward(el, arg);
        });
      case "xpath":
        return walk(nodes, function (el) {
          return xpath(el, arg);
        });
    }
    return [];
  }

  function select(ops) {
    var first = ops[0].type === "css-selector";
    var nodes = qsa(first ? ops[0].arg : "*");
    for (var i = first ? 1 : 0; i < ops.length && nodes.length > 0; i++) {
      nodes = step(nodes, ops[i]);
    }
    return nodes;
  }

  // No action means hide, which is what a plain rule does.
  //
  // Each of these reads the page's current state first, so a pass with nothing
  // to do changes nothing at all. That is what keeps the observer below from
  // waking on our own edits and looping — and, unlike remembering which
  // elements a rule has already touched, it also means that when the page
  // undoes what a rule did, the next pass does it again.
  function act(el, action) {
    if (!action) {
      if (el.style.getPropertyValue("display") !== "none") {
        el.style.setProperty("display", "none", "important");
      }
      return;
    }
    switch (action.type) {
      case "remove":
        el.remove();
        return;
      case "style":
        // Appended styles cannot be read back apart from the page's own, so
        // this one is remembered on the element — a property, not an
        // attribute, so it is not itself a mutation.
        if (el.__spStyle !== action.arg) {
          el.__spStyle = action.arg;
          el.style.cssText += ";" + action.arg;
        }
        return;
      case "remove-attr":
        if (el.hasAttribute(action.arg)) el.removeAttribute(action.arg);
        return;
      case "remove-class":
        if (el.classList.contains(action.arg)) el.classList.remove(action.arg);
        return;
    }
  }

  var pending = null;

  function run() {
    pending = null;
    for (var i = 0; i < FILTERS.length; i++) {
      var f = FILTERS[i];
      var nodes = select(f.selector);
      for (var j = 0; j < nodes.length; j++) {
        try {
          act(nodes[j], f.action);
        } catch (e) {}
      }
    }
  }

  function schedule() {
    if (pending === null) pending = setTimeout(run, DEBOUNCE_MS);
  }

  new MutationObserver(schedule).observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
  });
  run();
})();
