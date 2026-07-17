// Convert a uBlock Origin checkout's scriptlet library into the adblock-rust
// `Resource` JSON this proxy loads (config `adblock.scriptlet_resources`).
//
// Usage:
//   git clone --depth 1 https://github.com/gorhill/uBlock.git /tmp/ubo
//   node tools/convert-ubo-scriptlets.mjs /tmp/ubo lists/scriptlets.json
//
// Runs on Node (dev machines) AND on AWS LLRT (the tiny QuickJS runtime shipped
// in the release image so the admin UI's "Update from uBO" button works in a
// container). The three helpers below bridge LLRT's Node-API subset: it has no
// `fs.existsSync`/`fs.unlinkSync`/`process.on`, and its `pathToFileURL` won't
// absolutize a relative path (Node's does) — dynamic ESM import then rejects it.
//
// The generated file is uBO-derived and GPL — keep it as an operator-supplied
// data file; don't vendor it into this (MIT/Apache) source tree.
//
// How it works: uBO's newest resources register named functions via
// `registerScriptlet(fn, {name, aliases, dependencies:[fnRefs]})` (see
// base.js). Importing the ESM graph runs those side effects, and base.js
// already rewrites each dependency function-ref to its registered name. We read
// each function's canonical source with `fn.toString()` — no brittle text
// scraping — and emit the function-style resources adblock-rust 0.9.8 accepts:
//   * `.js`  scriptlets → kind application/javascript (injectable)
//   * `.fn`  helpers     → kind fn/javascript (dependency-only)
// Trusted scriptlets ARE included (default permission): the operator curates
// their own filter lists, so `trusted-*` rules (cookie / annoyance lists lean
// on them) should resolve. Also folds in uBO's web-accessible resources (the
// `##+js(nobab)` / `popads-dummy` stubs) with aliases from redirect-resources.js.
import fs from 'fs';
import path from 'path';
import url from 'url';

// Absolute file:// URL for a (possibly relative) path — LLRT's pathToFileURL
// leaves relatives relative, and its import() then errors "not absolute".
const fileUrl = (p) => url.pathToFileURL(path.resolve(p)).href;
// LLRT has no fs.existsSync; probe with statSync instead.
const exists = (p) => { try { fs.statSync(p); return true; } catch { return false; } };
// LLRT has neither unlinkSync nor process.on; best-effort, whichever exists.
const removeFile = (p) => { try { (fs.unlinkSync || fs.rmSync)?.(p); } catch {} };

const uboRoot = process.argv[2];  // a uBlock Origin checkout
const outPath = process.argv[3];
if (!uboRoot || !outPath) {
  console.error('usage: convert-ubo-scriptlets.mjs <uBO-checkout> <out.json>');
  process.exit(2);
}
const resDir = path.join(uboRoot, 'src/js/resources');
const warDir = path.join(uboRoot, 'src/web_accessible_resources');
const redirectMap = path.join(uboRoot, 'src/js/redirect-resources.js');

// The resource files are ESM; Node needs a `type:module` marker to import them
// (LLRT imports .js as ESM natively, but the marker is harmless there). Add one
// if absent; cleaned up at the end (and via exit on Node, in case of a throw).
const pkgPath = path.join(resDir, 'package.json');
const addedPkg = !exists(pkgPath);
if (addedPkg) fs.writeFileSync(pkgPath, '{"type":"module"}');
const cleanup = () => { if (addedPkg) removeFile(pkgPath); };
if (typeof process.on === 'function') process.on('exit', cleanup);

const EXT_MIME = {
  js: 'application/javascript', json: 'application/json', html: 'text/html',
  css: 'text/css', txt: 'text/plain', xml: 'text/xml',
};

// Import every module in the resources dir so all registerScriptlet() calls
// fire. ESM caches, so files imported transitively are only evaluated once.
const files = fs.readdirSync(resDir).filter((f) => f.endsWith('.js'));
for (const f of files) {
  await import(fileUrl(path.join(resDir, f)));
}
const { registeredScriptlets } = await import(fileUrl(path.join(resDir, 'base.js')));

const b64 = (s) => Buffer.from(s, 'utf8').toString('base64');

let skippedAnon = 0;
const resources = [];
for (const d of registeredScriptlets) {
  const src = d.fn.toString();
  const isDep = d.name.endsWith('.fn');
  // adblock-rust identifies function-style resources by a leading
  // `function name(` — verify injectable scriptlets match so they don't get
  // silently misread as `{{1}}` templates.
  if (!isDep && !/^function\s+[^(){}\s]+\s*\(/.test(src)) { skippedAnon++; continue; }
  resources.push({
    name: d.name,
    aliases: Array.isArray(d.aliases) ? d.aliases : [],
    kind: { mime: isDep ? 'fn/javascript' : 'application/javascript' },
    content: b64(src),
    dependencies: Array.isArray(d.dependencies)
      ? d.dependencies.filter((x) => typeof x === 'string')
      : [],
  });
}
const scriptletCount = resources.length - skippedAnon;

// Web-accessible resources: the neutered stubs (`nobab`, `popads-dummy`, …)
// that `##+js(...)` rules also invoke. Only text/JS kinds are injectable;
// images (`data: 'blob'`) are $redirect-only, so skip them.
let warCount = 0;
const aliasMap = (await import(fileUrl(redirectMap))).default;
for (const file of fs.readdirSync(warDir)) {
  const ext = file.split('.').pop();
  const mime = EXT_MIME[ext];
  if (mime !== 'application/javascript') continue; // scriptlet-injectable only
  const meta = aliasMap.get(file) || {};
  const aliases = Array.isArray(meta.alias) ? meta.alias : meta.alias ? [meta.alias] : [];
  resources.push({
    name: file,
    aliases,
    kind: { mime },
    content: b64(fs.readFileSync(path.join(warDir, file), 'utf8')),
    dependencies: [],
  });
  warCount++;
}

fs.writeFileSync(outPath, JSON.stringify(resources));
cleanup(); // explicit, so LLRT (no exit hook) also removes the temp marker
console.log(
  `wrote ${resources.length} resources: ${scriptletCount} scriptlets ` +
  `(incl. trusted) + deps, ${warCount} web-accessible stubs; skipped ${skippedAnon} anonymous`
);
