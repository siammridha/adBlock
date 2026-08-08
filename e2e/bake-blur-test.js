#!/usr/bin/env node
// Deterministic check that the bake's blur actually blurs. It pulls the real
// soften() out of the shipped blur_runtime.js and runs it in headless Chromium
// against a checkerboard, because a blur can only be judged on pixels.
//
// This is the check the bug that shipped needed: soften() replaced a canvas
// filter, which Safari ignores without complaint, so the bake wrote the picture
// back exactly as it came in and reported success. Nothing but reading the
// output pixels catches that.
//
// No proxy and no model — it runs the function on its own, so it needs nothing
// running.
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const CHROMIUM = process.env.CHROMIUM || "/usr/bin/chromium";
const SRC = path.join(__dirname, "..", "src", "adblock", "injected", "blur_runtime.js");
const src = fs.readFileSync(SRC, "utf8");

// Return the "{...}" block (braces included) that follows a header substring.
function block(header) {
  const h = src.indexOf(header);
  if (h < 0) throw new Error("could not find `" + header + "` in blur_runtime.js");
  const open = src.indexOf("{", h);
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}" && --depth === 0) return src.slice(open, i + 1);
  }
  throw new Error("unbalanced braces after `" + header + "`");
}

const soften = "function soften(bmp, W, H, radius) " + block("function soften(bmp, W, H, radius)");
const dataUrl = "function dataUrl(blob) " + block("function dataUrl(blob)");

// A 10px checkerboard: every neighbouring pair of pixels across a block edge is
// black against white, so the summed difference along a row is a plain measure
// of how much detail survived.
const page = `<html><body>PENDING<script>
var GRAY = false;
${soften}
${dataUrl}
(function () {
  var W = 400, H = 300;
  var board = new OffscreenCanvas(W, H), bc = board.getContext("2d");
  for (var i = 0; i < W / 10; i++) {
    for (var j = 0; j < H / 10; j++) {
      bc.fillStyle = (i + j) % 2 ? "#000" : "#fff";
      bc.fillRect(i * 10, j * 10, 10, 10);
    }
  }
  function detail(ctx) {
    var d = ctx.getImageData(100, 150, 60, 1).data, v = 0;
    for (var k = 1; k < 60; k++) v += Math.abs(d[k * 4] - d[(k - 1) * 4]);
    return v;
  }
  var raw = detail(bc);
  var blurred = detail(soften(board, W, H, 25).getContext("2d"));

  // The colour drain rides along on the same pass, so it is checked here too.
  var red = new OffscreenCanvas(20, 20), rc = red.getContext("2d");
  rc.fillStyle = "#c04030";
  rc.fillRect(0, 0, 20, 20);
  GRAY = true;
  var g = soften(red, 20, 20, 4).getContext("2d").getImageData(10, 10, 1, 1).data;
  var drained = Math.abs(g[0] - g[1]) < 3 && Math.abs(g[1] - g[2]) < 3;

  // The write-back path: what bake() hands the element is the canvas encoded as
  // a JPEG data url, not a handle to it.
  board.convertToBlob({ type: "image/jpeg", quality: 0.9 }).then(dataUrl).then(function (u) {
    document.body.textContent =
      "RESULT raw=" + raw + " blurred=" + blurred + " drained=" + drained +
      " baked=" + (u.lastIndexOf("data:image/jpeg;base64,", 0) === 0);
  });
})();
<\/script></body></html>`;

let dom;
try {
  dom = execFileSync(
    CHROMIUM,
    [
      "--headless",
      "--no-sandbox",
      "--disable-gpu",
      "--virtual-time-budget=8000",
      "--dump-dom",
      "data:text/html;base64," + Buffer.from(page).toString("base64"),
    ],
    { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }
  );
} catch (e) {
  console.log("FAIL could not run " + CHROMIUM + " — apt install chromium, or set CHROMIUM");
  process.exit(1);
}

const found = /RESULT raw=(\d+) blurred=(\d+) drained=(true|false) baked=(true|false)/.exec(dom);
if (!found) {
  console.log("FAIL soften() produced no result — the page threw");
  process.exit(1);
}
const raw = Number(found[1]);
const blurred = Number(found[2]);

let failed = 0;
function ok(name, cond) {
  console.log((cond ? "ok   " : "FAIL ") + name);
  if (!cond) failed = 1;
}
ok("the checkerboard has detail to lose (raw=" + raw + ")", raw > 500);
ok("soften() removes it (blurred=" + blurred + ", want under " + Math.round(raw / 10) + ")",
  blurred * 10 < raw);
ok("with grey on, the colour is drained", found[3] === "true");
ok("the bake comes out as a JPEG data url", found[4] === "true");

process.exit(failed);
