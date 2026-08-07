#!/usr/bin/env node
// Deterministic check of the <picture> bake path in blur_runtime.js: when an
// <img> inside a <picture> is baked, its <source> siblings must be detached (or
// they out-rank the baked src), and a reveal must put them back in order while a
// reset for a fresh picture must not. No browser and no ML model — it pulls the
// real bake write-back and unbake() out of the shipped file and runs them
// against a tiny fake DOM, so it tests the code that ships, not a copy.
const fs = require("fs");
const path = require("path");

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

const URL = { createObjectURL: () => "blob:baked", revokeObjectURL: () => {} };
const writeBack = new Function(
  "URL", "el", "blob",
  "return (function (blob) " + block(".then(function (blob)") + ")(blob);"
);
const unbake = new Function(
  "URL",
  "function unbake(el, restore) " + block("function unbake(el, restore)") + " return unbake;"
)(URL);

function picture(nSources) {
  const pic = { tagName: "PICTURE", kids: [] };
  pic.insertBefore = (node, ref) => {
    const i = pic.kids.indexOf(ref);
    pic.kids.splice(i < 0 ? pic.kids.length : i, 0, node);
    node.parentNode = pic;
  };
  pic.removeChild = (node) => {
    const i = pic.kids.indexOf(node);
    if (i >= 0) pic.kids.splice(i, 1);
    node.parentNode = null;
  };
  pic.querySelectorAll = (sel) =>
    pic.kids.filter((k) => k.tagName === sel.toUpperCase());
  const sources = [];
  for (let i = 0; i < nSources; i++) {
    const s = { tagName: "SOURCE", id: "s" + i, parentNode: pic };
    sources.push(s);
    pic.kids.push(s);
  }
  const img = image(pic);
  pic.kids.push(img);
  return { pic, img, sources };
}

function image(parent) {
  return {
    tagName: "IMG",
    parentNode: parent,
    src: "orig.jpg",
    style: {},
    _attrs: { srcset: "orig-2x.jpg 2x" },
    getAttribute(n) { return n in this._attrs ? this._attrs[n] : null; },
    setAttribute(n, v) { this._attrs[n] = v; },
    removeAttribute(n) { delete this._attrs[n]; },
  };
}

let failed = 0;
function ok(name, cond) {
  console.log((cond ? "ok   " : "FAIL ") + name);
  if (!cond) failed = 1;
}
const ids = (pic) => pic.kids.map((k) => k.id || k.tagName).join(",");

// 1. Bake an <img> in a <picture>, then reveal (restore=true).
{
  const { pic, img } = picture(2);
  writeBack(URL, img, {});
  ok("bake: src becomes the baked blob url", img.src === "blob:baked");
  ok("bake: srcset cleared", img.getAttribute("srcset") === null);
  ok("bake: <source> siblings detached", pic.querySelectorAll("source").length === 0);
  ok("bake: originals kept in order", ids(pic) === "IMG" &&
    img.__abOriginal.sources.map((s) => s.id).join(",") === "s0,s1");

  unbake(img, true);
  ok("reveal: <source> siblings reinserted before img, in order", ids(pic) === "s0,s1,IMG");
  ok("reveal: src restored", img.src === "orig.jpg");
  ok("reveal: srcset restored", img.getAttribute("srcset") === "orig-2x.jpg 2x");
  ok("reveal: bake state cleared", img.__abBaked === null && img.__abOriginal === null);
}

// 2. Bake, then reset for a fresh picture (restore=false): sources stay out,
//    because the page has already put the next picture on the node.
{
  const { pic, img } = picture(2);
  writeBack(URL, img, {});
  unbake(img, false);
  ok("reset: <source> siblings not reinserted", ids(pic) === "IMG");
  ok("reset: bake state cleared", img.__abBaked === null && img.__abOriginal === null);
}

// 3. A plain <img> (no <picture>): no sources to strip, restore still works.
{
  const div = { tagName: "DIV" };
  const img = image(div);
  writeBack(URL, img, {});
  ok("plain img: no sources recorded", img.__abOriginal.sources === null);
  ok("plain img: src baked", img.src === "blob:baked");
  unbake(img, true);
  ok("plain img: src restored", img.src === "orig.jpg");
  ok("plain img: srcset restored", img.getAttribute("srcset") === "orig-2x.jpg 2x");
}

process.exit(failed);
