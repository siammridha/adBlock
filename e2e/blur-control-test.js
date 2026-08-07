#!/usr/bin/env node
// Deterministic check of which of the two injected blur files starts the blur.
//
// On its own, blur_runtime.js starts itself: that is the normal page. Wrapped in
// debug_blur_runtime.js, it must start nothing until the panel's "run the blur"
// box is ticked — no stylesheet, no observer, no sweep — and ticking that box
// must start all three.
//
// No browser and no ML model — the two shipped files are put together the way
// Adblock puts them together and run against a tiny fake DOM, so it tests the
// code that ships, not a copy.
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const dir = path.join(__dirname, "..", "src", "adblock", "injected");
const RUNTIME = fs.readFileSync(path.join(dir, "blur_runtime.js"), "utf8");
const DEBUG = fs.readFileSync(path.join(dir, "debug_blur_runtime.js"), "utf8");

const SETTINGS = {
  __ROUTE_PREFIX__: "/__abx/",
  __BLUR_AMOUNT__: "40",
  __BLUR_STRICTNESS__: "40",
  __BLUR_MEN__: "true",
  __BLUR_WOMEN__: "true",
  __BLUR_IMAGES__: "true",
  __BLUR_VIDEOS__: "true",
  __BLUR_REGIONS__: "true",
  __BLUR_GRAY__: "false",
  __BLUR_ON_LOAD__: "true",
  __BLUR_HOVER_IMAGES__: "true",
  __BLUR_HOVER_VIDEOS__: "true",
  __BLUR_VERSION__: "deadbeef",
};

// The same two builds Adblock makes: the panel with the runtime spliced into it,
// or the runtime on its own.
function build(marks) {
  // A function replacement, so a `$` in the runtime is not read as a back-
  // reference the way a plain string replacement would read it.
  let js = marks ? DEBUG.replace("__BLUR_RUNTIME__", () => RUNTIME) : RUNTIME;
  js = js.split("__BLUR_MARKS__").join(marks ? "true" : "false");
  for (const [k, v] of Object.entries(SETTINGS)) js = js.split(k).join(v);
  assert(!js.includes("__BLUR_"), "a placeholder was left unreplaced");
  return js;
}

function node(tag) {
  return {
    tagName: tag.toUpperCase(),
    textContent: "",
    checked: false,
    style: { setProperty() {} },
    dataset: {},
    classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
    children: [],
    listeners: {},
    appendChild(kid) {
      this.children.push(kid);
      return kid;
    },
    insertBefore(kid) {
      this.children.push(kid);
      return kid;
    },
    removeChild() {},
    remove() {},
    addEventListener(type, fn) {
      (this.listeners[type] = this.listeners[type] || []).push(fn);
    },
    setPointerCapture() {},
    getBoundingClientRect: () => ({ left: 0, top: 0, right: 0, bottom: 0 }),
    querySelectorAll: () => [],
  };
}

// A page with nothing in it, and a count of everything the script does to it.
function page(js) {
  const head = node("head");
  const body = node("body");
  const seen = { observed: 0, disconnected: 0, sweeps: 0 };
  // The background sweep books a timer and only books another once that one has
  // come due, so the fake page has to be able to let them run.
  const timers = [];
  const document = {
    readyState: "complete",
    head,
    body,
    documentElement: node("html"),
    createElement: node,
    createTextNode: (t) => ({ nodeValue: t }),
    addEventListener() {},
    querySelectorAll: () => [],
    querySelector: () => null,
  };
  class Mutation {
    observe() {
      seen.observed++;
    }
    disconnect() {
      seen.disconnected++;
    }
  }
  class Intersection {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  const window = {
    createImageBitmap() {},
    Worker: function () {},
    addEventListener() {},
    innerWidth: 800,
    innerHeight: 600,
    getComputedStyle: () => ({ backgroundImage: "none" }),
  };
  new Function(
    "window,document,MutationObserver,IntersectionObserver,URL,location,setTimeout,requestAnimationFrame",
    js
  )(
    window,
    document,
    Mutation,
    Intersection,
    { createObjectURL: () => "blob:x", revokeObjectURL() {} },
    { origin: "https://example.test" },
    (fn) => {
      seen.sweeps++;
      timers.push(fn);
      return timers.length;
    },
    () => {}
  );
  return {
    head,
    body,
    seen,
    flush: () => {
      while (timers.length) timers.shift()();
    },
    // Everything the script has put in the page's head, as one string.
    styles: () => head.children.map((c) => c.textContent).join(""),
  };
}

// The runtime's own stylesheet, the one carrying the blur and the pre-verdict
// hold. A hold in a page with nothing running behind it never lets go, so this
// landing early is the thing to catch.
const HOLD = "abx-blur{filter:blur(";

// A normal page: no panel, and the runtime starts itself.
{
  const p = page(build(false));
  assert(p.styles().includes(HOLD), "with no panel the runtime puts its sheet in on load");
  assert(!p.styles().includes("data-ab-blur"), "and none of the panel's");
  assert.equal(p.body.children.length, 0, "nothing is added to the page itself");
  assert.equal(p.seen.observed, 1, "it watches the page");
  assert.equal(p.seen.sweeps, 1, "and sweeps it");
}

// A page being debugged: the panel is up and the runtime is holding off.
const p = page(build(true));
const panel = p.body.children.find((c) => c.id === "abx-blur-hud");
assert(panel, "the panel is put up on load, whether or not the blur is run");
assert(p.styles().includes("data-ab-blur"), "the panel's stylesheet is in");
assert(!p.styles().includes(HOLD), "the runtime's stylesheet must stay out until it is run");
assert.equal(p.seen.observed, 0, "nothing is watched before the blur is run");
assert.equal(p.seen.sweeps, 0, "and nothing is swept");

// [heading, run the blur, boxes, sample rate, settings, rows]
const runOn = panel.children[1].children[0];
assert.equal(runOn.checked, false, "the switch starts unticked");
function flip(on) {
  runOn.checked = on;
  runOn.listeners.change[0]();
}

flip(true);
assert(p.styles().includes(HOLD), "ticking it puts the runtime's sheet in");
assert.equal(p.seen.observed, 1, "and starts watching the page");
assert.equal(p.seen.sweeps, 1, "and sweeps it for backgrounds");
p.flush();

flip(false);
assert.equal(p.seen.disconnected, 1, "unticking it stops watching");

// Back on: the page is swept again, so whatever arrived while it was stopped is
// picked up, and neither stylesheet goes in a second time.
flip(true);
assert.equal(p.seen.observed, 2, "running it again watches the page again");
assert.equal(p.seen.sweeps, 2, "and sweeps it again");
assert.equal(p.head.children.length, 2, "the stylesheets go in once, not once per run");

console.log("blur control: ok");
