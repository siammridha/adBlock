// Blur the people in a page, on the way past.
//
// The model runs in a Worker built here from a blob URL, so the page keeps its
// own globals and nothing ever blocks the main thread. Frames reach it as
// ImageBitmaps, transferred rather than copied: no canvas, no JPEG, no
// re-decode. A still image is looked at once when it scrolls into view; a video
// is sampled on requestVideoFrameCallback, which only fires on a frame the
// browser actually presented.
//
// Images go through the model one at a time, and the one picked next is
// whichever is on screen at that moment, so what the reader is looking at is
// dealt with before the rest of the page.
//
// One model: HaramBlur's own. This branch exists to try it against the Human
// pipeline the other branch carries, so there is nothing to pick between here —
// the comparison is made by switching branches. It is a YOLOv8 that finds whole
// people and reads each one as Woman, Man, Girl or Person, so its box is the
// person and needs no growing, and there is no separate model for man or woman
// and no age.
//
// Everything else follows HaramBlur too: blur with or without the colour drained
// out, images and videos separately, every picture held back until a verdict
// lands and whether that hold is a blur or a hide, and the blur lifted while the
// pointer is over it. Which classes a switch hides is ours: the women switch
// takes Girl as well.
//
// How the blur is put on follows HaramBlur as well. With region blur off,
// everything is blurred whole by a CSS filter on the element itself. With region
// blur on, a still picture is baked: the people are blurred into a canvas and the
// result written back onto the element, its src or its background-image, so only
// they are blurred and nothing floats. The blur there is done by drawing the
// picture small and back up again, not by a canvas filter — Safari's canvas has
// no filter, and one asked for it there is ignored without complaint, which
// bakes the picture back exactly as it was. A video with region blur on cannot be
// baked frame by frame, so its people are covered by a floating box of blur
// patches that tracks them. That box sits over the video it covers, so it stays
// put; baking the still pictures is what stopped their box drifting on top of a
// video playing over a thumbnail.
//
// When the on-load switch is set, Adblock also injects a small stylesheet into
// the page's own <head>, ahead of this script, that blurs every img and video
// the moment it paints — so nothing flashes unblurred while the model is still
// downloading. This runtime rebuilds the same blanket rule and adds one more
// selector for CSS backgrounds. Both rules let go of an element the moment it
// gains the abx-blur-processed class, which mark() adds only on a real verdict
// — a picture the model looked at and cleared, blurred, or skipped as too
// small. A failure adds nothing, so a failed element stays covered.
//
// __BLUR_AMOUNT__, __BLUR_STRICTNESS__, __BLUR_MEN__, __BLUR_WOMEN__,
// __BLUR_IMAGES__, __BLUR_VIDEOS__, __BLUR_REGIONS__, __BLUR_GRAY__,
// __BLUR_ON_LOAD__, __BLUR_HOVER_IMAGES__, __BLUR_HOVER_VIDEOS__,
// and __BLUR_MARKS__ are replaced with Adblock's settings before this is
// injected, and __ROUTE_PREFIX__ with the path Adblock reserves on the page's
// own origin.
//
// Nothing here touches the page until run() is called. On its own this file
// calls it itself at the bottom. With the marks setting on, Adblock does not
// inject this file — it injects debug_blur_runtime.js with this one spliced
// inside it, and that file's panel decides when run() happens and when stop()
// does.
(function () {
  var AMOUNT = __BLUR_AMOUNT__;
  // Strictness runs 10–100, which is HaramBlur's own 0.1–1 written as a
  // percentage. In HaramBlur it moves an NSFW threshold, and there is no NSFW
  // model here, so it moves the one bar this pipeline has instead: how sure the
  // detector must be that it found a person. Higher strictness, lower bar, more
  // blurring. 40 is the default both here and there, and lands exactly on
  // HaramBlur's own bar for this model, 0.35.
  var STRICTNESS = __BLUR_STRICTNESS__;
  var SCORE = Math.max(0.05, (0.35 * (100 - STRICTNESS)) / 60);
  var MEN = __BLUR_MEN__;
  var WOMEN = __BLUR_WOMEN__;
  var IMAGES = __BLUR_IMAGES__;
  var VIDEOS = __BLUR_VIDEOS__;
  var REGIONS = __BLUR_REGIONS__;
  var GRAY = __BLUR_GRAY__;
  var ON_LOAD = __BLUR_ON_LOAD__;
  var HOVER_IMAGES = __BLUR_HOVER_IMAGES__;
  var HOVER_VIDEOS = __BLUR_HOVER_VIDEOS__;
  var MARKS = __BLUR_MARKS__;
  // Whether the blur is running. Off until run() is called, and off again after
  // stop(); every path that queues a picture or sends a frame to the model asks
  // this first.
  var live = false;
  // Whether the one-time page furniture — the stylesheet and the layout
  // listeners — has been put in. A stop leaves it, so a second run does not add
  // it again.
  var wired = false;
  var CLASS = "abx-blur";
  // A picture with a side under this many of its own pixels is not looked at.
  // Icons, bullets, spacers and tracking pixels are most of what a page carries
  // and none of them can hold a person, but each one costs a run of the
  // detector. HaramBlur's own cutoff, and not a setting for the same reason the
  // model is not: this branch is here to run HaramBlur's numbers.
  var MIN_SIZE = 50;
  var SAMPLE_MS = 1000 / 25; // a video is sampled at most this often
  var SCAN_MS = 500; // the page is swept for background pictures at most this often
  // HaramBlur's own hysteresis on a video: two frames with somebody in them
  // before it blurs, three without before it clears again. A person lost for
  // one frame is a blink or a turn, and a person found in one is as often the
  // detector twitching.
  var BLUR_RUN = 2;
  var CLEAR_RUN = 3;
  // How many frames in a row have to fail before a video is given up on. One
  // failure is a hiccup — a decoder stall, the worker rebuilding its canvas
  // after a bad frame, a graphics context lost and restored — and HaramBlur logs
  // one and carries on sampling. Stopping on the first one is what left a patch
  // frozen over a video that went on playing underneath it.
  var FAIL_RUN = 3;
  // A frame the video has already moved this far past is thrown away rather
  // than acted on: covering where somebody was half a second ago is worse than
  // covering nothing.
  var STALE_S = 0.5;

  if (!window.createImageBitmap || !window.Worker) return;

  // TensorFlow.js from a CDN, and the detector's weights from Adblock. About
  // 1 MB of library and 11 MB of weights, downloaded on the first page that has
  // a picture worth looking at; the browser caches both after that.
  //
  // The weights do not come from a CDN because there is no CDN for them — they
  // are HaramBlur's own files, so Adblock serves them out of its own store, on
  // the page's own address. The worker is built from a string and loads them by
  // absolute URL, because a blob worker has no page address to resolve against.
  //
  // ponytail: the library is fetched from someone else's host at page load.
  // Adblock already serves the weights; put the library beside them if that ever
  // matters.
  var TFJS = "https://cdn.jsdelivr.net/npm/@tensorflow/tfjs@4.22.0/dist/tf.min.js";
  var MODEL_BASE = location.origin + "__ROUTE_PREFIX__blur-model/";

  // The model as HaramBlur exports and configures it: a 640x640 input, four box
  // numbers and four class scores per candidate, and its own bars for how much
  // two boxes may overlap and how many people it will report at once.
  var MODEL_SIZE = 640;
  // How small a frame is made before the model is handed it, longest side.
  // HaramBlur's two caps: a picture at the model's own input size, a video frame
  // an eighth larger. Its floor of 512 never bites, because this model is 640.
  var IMAGE_CAP = MODEL_SIZE;
  var VIDEO_CAP = Math.round(MODEL_SIZE * 1.125);
  // The picture is baked at close to full size, not at the model's cap: the
  // people are blurred but the rest stays sharp, and softening the whole thing
  // to 640 to save a downscale would give the sharp part away. Bounded so a huge
  // original does not build a canvas out of all proportion to what is on screen.
  var BAKE_CAP = 2048;
  var CLASSES = ["woman", "man", "girl", "person"];
  var MAX_DETECTED = 70;
  var IOU = 0.7;

  // The worker: hand it a frame, get back one entry per person found.
  //
  // The frame is padded out to a square at the top left and scaled into the
  // model's input, which is what HaramBlur does. A picture stretched into a
  // square instead would reach the detector with people no longer shaped like
  // people. How much padding was added is kept, so the boxes can be put back
  // onto the frame afterwards.
  //
  // The model's head is raw: it emits every candidate it has, with nothing
  // suppressed and no box assembled, so that is done here. Step for step this is
  // HaramBlur's own decode — centre and size to corners, best class per
  // candidate, then non-max suppression on the score.
  var WORKER_SRC = [
    "importScripts('" + TFJS + "');",
    "var SIZE = " + MODEL_SIZE + ", CLASSES = " + JSON.stringify(CLASSES) + ";",
    "var MAX_DETECTED = " + MAX_DETECTED + ", IOU = " + IOU + ", SCORE = " + SCORE + ";",
    "var canvas = new OffscreenCanvas(SIZE, SIZE);",
    "var ctx = canvas.getContext('2d', { willReadFrequently: true });",
    "var model = null;",
    "var ready = tf.ready()",
    "  .then(function () { return tf.loadGraphModel('" + MODEL_BASE + "model.json'); })",
    "  .then(function (m) {",
    "    model = m;",
    "    postMessage({ ready: true, backend: tf.getBackend() });",
    "  }, function (e) {",
    "    postMessage({ ready: false, why: String((e && e.message) || e) });",
    "    throw e;",
    "  });",
    "onmessage = function (e) {",
    "  var id = e.data.id;",
    "  var bmp = e.data.bmp, w = bmp.width, h = bmp.height;",
    "  ready.then(function () {",
    // A square as wide as the longer side, the frame in its top-left corner and
    // the rest left black.
    "    var side = Math.max(w, h), scale = SIZE / side;",
    "    ctx.clearRect(0, 0, SIZE, SIZE);",
    "    ctx.drawImage(bmp, 0, 0, w * scale, h * scale);",
    "    bmp.close();",
    "    var pixels = ctx.getImageData(0, 0, SIZE, SIZE);",
    // [1, 4 + classes, N] comes out, a candidate per column. Transposed so that
    // a candidate is a row, which is what every slice below reads.
    "    var rows = tf.tidy(function () {",
    "      var input = tf.browser.fromPixels(pixels).toFloat().div(255).expandDims(0);",
    "      return model.execute(input).transpose([0, 2, 1]);",
    "    });",
    "    var boxes = tf.tidy(function () {",
    "      var bw = rows.slice([0, 0, 2], [-1, -1, 1]);",
    "      var bh = rows.slice([0, 0, 3], [-1, -1, 1]);",
    "      var x = tf.sub(rows.slice([0, 0, 0], [-1, -1, 1]), tf.div(bw, 2));",
    "      var y = tf.sub(rows.slice([0, 0, 1], [-1, -1, 1]), tf.div(bh, 2));",
    // Suppression takes each box top edge first, not left edge first.
    "      return tf.concat([y, x, tf.add(y, bh), tf.add(x, bw)], 2).squeeze();",
    "    });",
    "    var best = tf.tidy(function () {",
    "      var c = rows.slice([0, 0, 4], [-1, -1, CLASSES.length]).squeeze(0);",
    "      return [c.max(1), c.argMax(1)];",
    "    });",
    "    rows.dispose();",
    // Nothing here is inside a tidy, because suppression is asynchronous. Every
    // tensor made along the way is listed as it is made and the whole list is
    // dropped at the end, whether the frame worked or not — a leak here is a
    // leak per frame, and a video brings five a second.
    "    var junk = [boxes, best[0], best[1]];",
    "    return tf.image",
    "      .nonMaxSuppressionAsync(boxes, best[0], MAX_DETECTED, IOU, SCORE)",
    "      .then(function (keep) {",
    "        var got = [boxes.gather(keep, 0), best[0].gather(keep, 0), best[1].gather(keep, 0)];",
    "        junk.push(keep, got[0], got[1], got[2]);",
    "        return Promise.all([got[0].array(), got[1].array(), got[2].array()]);",
    "      })",
    "      .then(function (got) {",
    // Back off the padded square and onto the frame, as fractions of it, which
    // is how the page places a patch.
    "        var xr = side / (SIZE * w), yr = side / (SIZE * h);",
    "        return got[0].map(function (b, i) {",
    "          return {",
    "            gender: CLASSES[got[2][i]],",
    "            score: got[1][i],",
    // No age: this model does not read one, and null is how the page is told to
    // leave that gate alone rather than treat everyone as a newborn.
    "            age: null,",
    "            face: got[1][i],",
    "            box: [b[1] * xr, b[0] * yr, b[3] * xr, b[2] * yr],",
    "          };",
    "        });",
    "      })",
    "      .finally(function () { tf.dispose(junk); });",
    "  }).then(function (out) {",
    "    postMessage({ id: id, out: out });",
    "  }).catch(function (err) {",
    // A frame the page is not allowed to read poisons the canvas it was drawn
    // on, and every frame after it fails the same way on the same canvas — one
    // cross-origin video would take every picture on the page down with it. So
    // the canvas is thrown away on any failure rather than reused.
    "    canvas = new OffscreenCanvas(SIZE, SIZE);",
    "    ctx = canvas.getContext('2d', { willReadFrequently: true });",
    "    postMessage({ id: id, out: null, why: String((err && err.message) || err) });",
    "  });",
    "};",
  ].join("\n");

  // What a blur looks like, in one string, so the element filter, the patch
  // backdrop and the load animation cannot drift apart. HaramBlur's: a radius,
  // and the colour drained out with it if that is switched on.
  var FILTER = "blur(" + AMOUNT + "px)" + (GRAY ? " grayscale(100%)" : "");

  // The pre-verdict hold. Every picture starts covered and stays covered until a
  // verdict adds abx-blur-processed to it; nothing here has a clock. The proxy's
  // on-load stylesheet writes the same img/video rule ahead of this script; this
  // one rebuilds it and adds CSS backgrounds, which the runtime marks with
  // data-ab-hold once it finds a real url() on them. Same switches the proxy
  // uses: images cover img and backgrounds, videos cover video.
  var HOLD = [];
  if (IMAGES) HOLD.push("img:not(." + CLASS + "-processed)");
  if (VIDEOS) HOLD.push("video:not(." + CLASS + "-processed)");
  if (IMAGES) HOLD.push("[data-ab-hold]:not(." + CLASS + "-processed)");

  var sheet = document.createElement("style");
  sheet.textContent =
    "." + CLASS + "{filter:" + FILTER + "!important;transition:filter .1s ease!important}" +
    // A region cover is a box laid over a video, holding one patch per person to
    // hide. The patches blur what is behind them rather than the element, so the
    // rest of the frame stays as it was. Only video uses it now — a still picture
    // with region blur on is baked instead.
    // No z-index here: a patch layer is stacked where the video it covers is
    // stacked, which is only known per video, so it is set as one is placed.
    "." + CLASS + "-box{position:absolute;pointer-events:none}" +
    // A patch is put where it is and does not travel there. HaramBlur's
    // stylesheet eases one over .3s, but nothing there is ever eased: it throws
    // its patches away and builds new ones on every update, with the position
    // already set before they go in, and a new element has nothing to move from.
    // Patches here are moved instead, so that rule would really run, and a patch
    // easing across a video trails a second behind the person it is covering.
    "." + CLASS + "-box>i{position:absolute;border-radius:5px;" +
    "background:rgba(255,255,255,.2);box-shadow:0 4px 30px rgba(0,0,0,.1);" +
    "backdrop-filter:" + FILTER + ";-webkit-backdrop-filter:" + FILTER + "}" +
    // The per-video toggle. Placed by script at the video's top-right corner
    // (top/right here would be against its containing block, not the video), so
    // left/top are set as it is positioned. A z-index above everything: it is a
    // control the reader clicks, not a cover.
    "." + CLASS + "-toggle{position:absolute;z-index:2147483647;margin:0;" +
    "padding:3px 8px;font:600 11px/1.4 system-ui,sans-serif;color:#fff;" +
    "background:rgba(0,0,0,.6);border:0;border-radius:12px;cursor:pointer;" +
    "opacity:.85;pointer-events:auto;user-select:none;-webkit-user-select:none}" +
    "." + CLASS + "-toggle:hover{opacity:1}" +
    // The hold: blurred if the on-load switch says so, hidden outright if it
    // does not. Lets go the moment a verdict adds abx-blur-processed. A hold
    // that ran out on its own would show a picture nobody had looked at yet,
    // which is the one thing this is for, so there is no clock — a failure never
    // adds the class, so a picture the model could not read stays covered.
    // Written before the hover rule so that, where the two match at the same
    // specificity (a background div), the hover lift below wins on source order.
    (HOLD.length
      ? HOLD.join(",") +
        "{" +
        (ON_LOAD ? "filter:" + FILTER : "visibility:hidden") +
        "!important}"
      : "") +
    // The blur lifted while the pointer is over the picture. A picture blurred by
    // a filter on the element is lifted by that class. A baked picture is lifted
    // by script, swapping the original back in, since its blur is in the pixels
    // not the CSS. A video's patch box is not a child of the video and so cannot
    // be reached by a hover rule on it; script toggles the class below and this
    // hides the box. HaramBlur eases it off over half a second and waits a second
    // first, so brushing past a picture does not flash it.
    (HOVER_IMAGES || HOVER_VIDEOS
      ? "." + CLASS + "." + CLASS + "-off{filter:none!important;" +
        "transition:filter .5s ease!important;transition-delay:1s!important}" +
        "." + CLASS + "-box." + CLASS + "-off{display:none!important}" +
        // A held picture the pointer is over still lifts, before any verdict.
        // Ties the background hold rule on specificity, so it is written after
        // it to win on source order; it beats the img/video hold outright.
        ":not(." + CLASS + "-processed)." + CLASS + "-off{visibility:visible!important;" +
        "filter:none!important}"
      : "");
  // Not put in the page here: the sheet carries the hold, and a hold with
  // nothing running behind it never lets go. run() appends it.

  // Every verdict passes through here, which is why the hold comes off here.
  // The overlay is told the same thing, when there is one.
  function mark(el, state, note) {
    // The hold comes off only on a real verdict: the model looked at the picture
    // and cleared it, blurred it, or skipped it as too small. A failure — the
    // frame could not be read, or there is no model — is not a verdict and adds
    // nothing, so the hold keeps that element covered. Done here because every
    // one of those paths reports itself through mark() already, and this runs
    // whether or not marks are on.
    if (state === "clear" || state === "blurred" || state === "skipped") {
      el.classList.add(CLASS + "-processed");
    }
    if (MARKS) report(el, state, note);
  }

  // The Worker is not started until something is actually asked of it, so a
  // page with no pictures at all downloads nothing.
  var worker = null;
  var broken = false;
  var seq = 0;
  var waiting = Object.create(null);

  // Nothing more will be answered: everyone still waiting is told so, rather
  // than left hanging on a promise that will never settle. `null` means "could
  // not tell", and nothing is ever blurred on it — never blur because a model
  // broke.
  function giveUp() {
    broken = true;
    for (var id in waiting) waiting[id](null);
    waiting = Object.create(null);
  }

  function start() {
    if (broken) return null;
    if (worker) return worker;
    try {
      var url = URL.createObjectURL(new Blob([WORKER_SRC], { type: "text/javascript" }));
      worker = new Worker(url);
      URL.revokeObjectURL(url);
      worker.onmessage = function (e) {
        // The worker says once whether its models came up, so a page can tell
        // "it never loaded" apart from "nobody to blur here".
        if (e.data.ready !== undefined) {
          if (e.data.ready) {
            if (MARKS) {
              ranOn(e.data.backend);
              console.log("blur: ready on " + e.data.backend);
            }
            return;
          }
          if (MARKS) {
            ranOn("failed");
            console.warn("blur: could not load —", e.data.why);
          }
          giveUp();
          return;
        }
        if (MARKS && e.data.why) console.warn("blur: failed —", e.data.why);
        var cb = waiting[e.data.id];
        delete waiting[e.data.id];
        if (cb) cb(e.data.out);
      };
      worker.onerror = function (e) {
        if (MARKS) console.warn("blur: failed to start —", e.message || e);
        giveUp();
      };
    } catch (e) {
      broken = true;
      if (MARKS) console.warn("blur: no worker", e);
      return null;
    }
    return worker;
  }

  // Hand over a frame, get back one entry per person found, or null if the
  // model could not answer.
  function findPeople(bmp) {
    return new Promise(function (resolve) {
      var w = start();
      if (!w) {
        bmp.close();
        resolve(null);
        return;
      }
      var id = ++seq;
      waiting[id] = resolve;
      w.postMessage({ id: id, bmp: bmp }, [bmp]);
    });
  }

  // Whether a picture is too small to be worth the detector's time. Its own
  // pixels, not the size it is drawn at: a large picture squeezed into a corner
  // still has a person in it, and blurring it has to blur the whole thing.
  function tooSmall(w, h) {
    return w < MIN_SIZE || h < MIN_SIZE;
  }

  // Whether this box is one of the ones to hide. Girl goes with Woman on the
  // same switch: the model splits them by how old the body looks, which is not a
  // distinction the switch is asking about, and a girl left showing while every
  // woman beside her is covered is the switch not doing what it says. This is
  // where we part from HaramBlur, which lets a child through on its age gate.
  //
  // Person is somebody the model could not read either way, and those are left
  // alone rather than hide a body on no evidence about who it is.
  //
  // The score is not compared here. It is the detector's own confidence, and the
  // bar it had to clear was applied inside the worker; a box that came back at
  // all has cleared it.
  function wanted(f) {
    return (
      (f.gender === "man" && MEN) ||
      ((f.gender === "woman" || f.gender === "girl") && WOMEN)
    );
  }

  // The longest side brought down to the cap, the shape kept, and anything
  // already inside the cap left exactly as it is — HaramBlur's own resize, step
  // for step. Nothing is smoothed on the way down, which is what its canvas does
  // with `imageSmoothingEnabled` off.
  //
  // Nothing to do means no options at all rather than the same numbers again: a
  // resize asked for is a resize done, even when it lands on the size it started
  // at.
  function fit(w, h, cap) {
    var side = Math.max(w, h);
    if (!side || side <= cap) return undefined;
    var scale = cap / side;
    return {
      resizeWidth: Math.round(w * scale),
      resizeHeight: Math.round(h * scale),
      resizeQuality: "pixelated",
    };
  }

  // A picture is re-requested with an anonymous fetch, because one the page
  // loaded normally is tainted and cannot be read back; Adblock allows that
  // read on the response side while the blur is on, and the browser answers the
  // second request from its cache.
  function fromUrl(url, cap) {
    return new Promise(function (resolve, reject) {
      var probe = new Image();
      probe.crossOrigin = "anonymous";
      probe.onload = function () {
        resolve(
          createImageBitmap(probe, fit(probe.naturalWidth, probe.naturalHeight, cap))
        );
      };
      probe.onerror = reject;
      probe.src = url;
    });
  }

  // Grab the frame, shrunk on the way to the cap for its kind, which is where
  // HaramBlur shrinks it too: a 4K frame carried over whole and then scaled to
  // 640 inside the worker is megabytes moved and a downscale done twice, and the
  // model never sees any of that detail either way.
  //
  // Videos are sampled straight off the element — a player feeding an MSE blob
  // is already same-origin — and only while playing, the same as HaramBlur: a
  // video is never read before it plays, so its poster and its parked first
  // frame are never looked at.
  //
  // A background picture is fetched instead of read off an element: there is no
  // element behind it to report a natural size, so its own size is kept off the
  // bitmap and stands in for it, or every patch is laid out as if it filled the
  // box and lands beside the person instead of on them. Shrinking keeps the
  // shape, so it is still the ratio it is laid out by.
  function grab(el) {
    if (el.tagName === "VIDEO") {
      return createImageBitmap(el, fit(el.videoWidth, el.videoHeight, VIDEO_CAP));
    }
    if (el.__abBg) return fromUrl(el.__abBg, IMAGE_CAP).then(sized(el));
    return fromUrl(el.src, IMAGE_CAP);
  }

  function sized(el) {
    return function (bmp) {
      el.__abStill = { w: bmp.width, h: bmp.height };
      return bmp;
    };
  }

  // Everything the model is given passes through here, so with marking on the
  // panel can show each frame at the size it actually went over as.
  function snapshot(el) {
    if (!MARKS) return grab(el);
    return grab(el).then(function (bmp) {
      thumb(el, bmp);
      return bmp;
    });
  }

  // What to actually cover: the box as the model drew it. This detector returns
  // the whole person, so there is nothing to guess about where the body went and
  // nothing to grow. All four numbers are fractions of the picture.
  function area(b) {
    return { x: b[0], y: b[1], w: b[2] - b[0], h: b[3] - b[1] };
  }

  // A layer goes where HaramBlur puts it: in the picture's own parent, right
  // before the picture, one step above it in the stack. Each picture can carry
  // two — the blur patches, and the debugging outlines. A video gets both moved
  // on every sample, which is how they follow the people in it.
  //
  // On document.body instead, a layer is in the page's top-level stack, and its
  // own z-index means nothing there: over a player it covers the controls, and
  // under a container that has a z-index of its own it disappears behind the
  // picture it is meant to cover. Neither can happen from inside the picture's
  // own parent, where the picture's own stacking order is what it is measured
  // against. Nothing is wrapped and nothing moves: an absolutely positioned
  // sibling takes no room, so no page layout is touched either way.
  //
  // ponytail: a layer follows scrolling for free now that it sits beside the
  // picture, and follows resizing because a resize re-places it. A picture moved
  // by script, by an animation, or by being sticky drifts away from its layers
  // until the next resize; put a ResizeObserver on the host if that shows up.
  var covers = [];
  var placing = false;

  function drop(entry) {
    if (entry.box) entry.box.remove();
    if (entry.marks) entry.marks.remove();
    entry.host.__abCover = null;
    covers.splice(covers.indexOf(entry), 1);
  }

  // A box came back as a fraction of the frame, and the frame is not the
  // element: a video is drawn letterboxed inside its box, because object-fit is
  // contain for a video by default. A layer sized to the element then puts every
  // patch off to one side by the width of the bar. So both are worked out — where
  // the frame is drawn, and the box it shows through. A letterboxed frame sits
  // inside its box, a cropped one runs past it and is cut off by it.
  //
  // The box is the content box, not what getBoundingClientRect gives back: a
  // border or padding on the element holds the frame in by that much, and taking
  // the outer rect for it slides every patch up and to the left.
  //
  // Where in the box the frame is put is read as well, because a page that moves
  // it moves the frame without moving the element: `object-position: center top`
  // on a portrait in a square hole slides the whole frame up, and every patch
  // with it. Centred is only the default.
  function drawn(el) {
    var r = el.getBoundingClientRect();
    var css = window.getComputedStyle(el);
    var l = px(css.borderLeftWidth) + px(css.paddingLeft);
    var t = px(css.borderTopWidth) + px(css.paddingTop);
    var box = {
      left: r.left + l,
      top: r.top + t,
      width: Math.max(0, r.width - l - px(css.borderRightWidth) - px(css.paddingRight)),
      height: Math.max(0, r.height - t - px(css.borderBottomWidth) - px(css.paddingBottom)),
    };
    // A video is measured by its decoded frame; a background picture by its own
    // size, taken when it was read, since the div behind it reports none.
    var still = el.__abStill;
    var iw = el.videoWidth || el.naturalWidth || (still && still.w) || 0;
    var ih = el.videoHeight || el.naturalHeight || (still && still.h) || 0;
    var how = el.__abBg ? bgFit(css.backgroundSize) : css.objectFit || "fill";
    if (!iw || !ih || !box.width || !box.height || how === "fill") {
      return { frame: box, box: box };
    }
    var scale = how === "cover"
      ? Math.max(box.width / iw, box.height / ih)
      : Math.min(box.width / iw, box.height / ih);
    if (how === "none") scale = 1;
    if (how === "scale-down") scale = Math.min(scale, 1);
    var w = iw * scale;
    var h = ih * scale;
    var pos = ((el.__abBg ? css.backgroundPosition : css.objectPosition) || "50% 50%").split(" ");
    return {
      frame: {
        left: box.left + along(pos[0], box.width - w),
        top: box.top + along(pos[1] === undefined ? pos[0] : pos[1], box.height - h),
        width: w,
        height: h,
      },
      box: box,
    };
  }

  // One axis of object-position: a percentage is of the room left over, so 50% is
  // centred and 100% is against the far edge; anything else is a length from the
  // near edge. The room is negative for a frame wider than its box, which is what
  // pulls a cropped frame out past the edge.
  function along(v, room) {
    return v.slice(-1) === "%" ? (parseFloat(v) / 100) * room : px(v);
  }

  function px(v) {
    return parseFloat(v) || 0;
  }

  // A background is fitted into its box by its own property, which says two of
  // the same things under the same names: cover and contain mean what they mean
  // for a picture.
  //
  // ponytail: nothing else is followed. A background left at auto, or given a
  // pair of lengths, is laid out as if it filled the box — patches on one of
  // those are placed by the box rather than by the picture. Carry the natural
  // size off the probe in fromUrl() if a page turns up where that shows.
  function bgFit(v) {
    return v === "cover" || v === "contain" ? v : "fill";
  }

  // Which slice of the frame, as a fraction of it, is on the picture. A frame
  // drawn inside its box is all of it: 0 to 1.
  function seen(lo, hi, start, size) {
    if (!size) return [0, 1];
    return [frac((lo - start) / size), frac((hi - start) / size)];
  }

  function frac(v) {
    return v < 0 ? 0 : v > 1 ? 1 : v;
  }

  // Layers are sized to the frame, and every patch on one is trimmed to the part
  // of the frame that can be seen. The trim is done on the patches and not with
  // a clip on the layer, because a clipped layer is a backdrop root, and a patch
  // inside one of those has nothing behind it left to blur.
  function placeAll() {
    placing = false;
    for (var i = covers.length - 1; i >= 0; i--) {
      var entry = covers[i];
      if (!entry.host.isConnected) {
        drop(entry);
        continue;
      }
      var d = drawn(entry.host);
      var r = d.frame;
      // A picture the page has taken away takes its layers with it. They are
      // its siblings, not its children, so hiding it hides nothing of theirs on
      // its own: a player's poster swapped out for the video would otherwise
      // keep its patches and its outlines painted over the video now playing
      // where it was, on top of that video's own.
      var css = window.getComputedStyle(entry.host);
      var gone = !r.width || !r.height || css.visibility === "hidden";
      if (entry.box) entry.box.style.display = gone ? "none" : "";
      if (entry.marks) entry.marks.style.display = gone ? "none" : "";
      if (gone) continue;
      var vx = seen(d.box.left, d.box.left + d.box.width, r.left, r.width);
      var vy = seen(d.box.top, d.box.top + d.box.height, r.top, r.height);
      // A layer is placed against its own containing block, which is not always
      // the corner the picture is placed against: an image in a plain table cell
      // is offset from the table, but the layer, absolute with no positioned
      // ancestor, is offset from the page. So the layer is parked at its own
      // origin and measured to find where that origin lands in the viewport, and
      // the frame — also in viewport coordinates — is stepped to from there. This
      // holds however the page nests the picture. Both layers share the picture's
      // parent, so one measurement serves both.
      var probe = entry.box || entry.marks;
      probe.style.left = "0px";
      probe.style.top = "0px";
      var o = probe.getBoundingClientRect();
      var x0 = r.left - o.left;
      var y0 = r.top - o.top;
      // One step above the picture, which is HaramBlur's rule. Anything the page
      // stacks over the picture — a play button, a caption, its own controls —
      // is above that and stays over the blur, instead of being blurred with it.
      var depth = css.zIndex;
      var keys = ["box", "marks"];
      for (var k = 0; k < 2; k++) {
        var layer = entry[keys[k]];
        if (!layer) continue;
        var s = layer.style;
        s.left = x0 + "px";
        s.top = y0 + "px";
        s.width = r.width + "px";
        s.height = r.height + "px";
        if (keys[k] === "box") s.zIndex = (depth === "auto" ? 0 : +depth || 0) + 1;
        var cells = entry.cells[keys[k]];
        for (var c = 0; c < cells.length; c++) {
          var cell = cells[c];
          var x = Math.max(cell.x, vx[0]);
          var y = Math.max(cell.y, vy[0]);
          var cs = layer.children[c].style;
          cs.left = x * 100 + "%";
          cs.top = y * 100 + "%";
          cs.width = Math.max(0, Math.min(cell.x + cell.w, vx[1]) - x) * 100 + "%";
          cs.height = Math.max(0, Math.min(cell.y + cell.h, vy[1]) - y) * 100 + "%";
        }
      }
    }
  }

  function reflow() {
    if (placing) return;
    placing = true;
    requestAnimationFrame(placeAll);
  }

  // Lay one child per cell over the element. Children are reused rather than
  // rebuilt, so a video being re-covered several times a second does not churn
  // the DOM. The cells are kept, because where a patch goes depends on the
  // element's size as much as on the cell, and that changes without the cells
  // changing. No cells means the layer goes away.
  function lay(el, key, cls, tag, cells) {
    var entry = el.__abCover;
    if (!cells.length && (!entry || !entry[key])) return;
    if (!entry) {
      entry = { host: el, box: null, marks: null, cells: { box: [], marks: [] } };
      el.__abCover = entry;
      covers.push(entry);
    }
    entry.cells[key] = cells;
    if (!cells.length) {
      entry[key].remove();
      entry[key] = null;
      if (!entry.box && !entry.marks) drop(entry);
      return;
    }
    var layer = entry[key];
    if (!layer) {
      layer = document.createElement("div");
      layer.className = cls;
      (el.parentNode || document.body).insertBefore(layer, el);
      entry[key] = layer;
    }
    while (layer.children.length > cells.length) layer.removeChild(layer.lastChild);
    while (layer.children.length < cells.length) {
      layer.appendChild(document.createElement(tag));
    }
    for (var i = 0; i < cells.length; i++) {
      if (cells[i].label !== undefined) layer.children[i].textContent = cells[i].label;
    }
    placeAll();
  }

  // How many of the people found were ones to hide, for the tooltip.
  function tally(picked, total) {
    return picked + " of " + total + (total === 1 ? " person" : " people");
  }

  // Two people standing together come back as two boxes that overlap. They are
  // folded into one rectangle around both, so the bake copies one region instead
  // of two that share a strip. HaramBlur's own merge, including that it is one
  // pass: a box is compared against what has been taken so far and nothing is
  // reconsidered afterwards.
  function merge(cells) {
    var out = [];
    for (var i = 0; i < cells.length; i++) {
      var cell = cells[i];
      var into = -1;
      for (var j = 0; j < out.length; j++) {
        if (overlap(cell, out[j])) {
          into = j;
          break;
        }
      }
      if (into < 0) {
        out.push(cell);
        continue;
      }
      var x = Math.min(cell.x, out[into].x);
      var y = Math.min(cell.y, out[into].y);
      out[into] = {
        x: x,
        y: y,
        w: Math.max(cell.x + cell.w, out[into].x + out[into].w) - x,
        h: Math.max(cell.y + cell.h, out[into].y + out[into].h) - y,
      };
    }
    return out;
  }

  function overlap(a, b) {
    return !(
      a.x + a.w <= b.x ||
      b.x + b.w <= a.x ||
      a.y + a.h <= b.y ||
      b.y + b.h <= a.y
    );
  }

  // A box of blur patches laid over a video, one patch per person. No cells
  // takes the box away.
  function cover(el, cells) {
    lay(el, "box", CLASS + "-box", "i", cells);
  }

  function uncover(el) {
    lay(el, "box", CLASS + "-box", "i", []);
  }

  // Hide the people picked out of a frame. With region blur off, the whole
  // element is blurred by a filter on itself — no float, it lives in the
  // element's own stacking place. With region blur on: a still picture is baked
  // (the blur drawn into the pixels and written back onto the element, so only
  // the people are blurred and still nothing floats), and a video, which cannot
  // be baked frame by frame, gets a floating box of patches over its people
  // instead. That box sits over the video it covers, so it never drifts onto
  // something else the way an image's box once drifted onto a video. Returns a
  // promise, so the caller can wait for a bake before lifting the hold — the CSS
  // and box paths resolve at once, the picture already blurred by the time the
  // hold goes.
  function hide(el, picked) {
    if (!REGIONS) {
      el.classList.add(CLASS);
      return Promise.resolve();
    }
    var cells = merge(
      picked.map(function (f) {
        return area(f.box);
      })
    );
    if (el.tagName === "VIDEO") {
      el.classList.remove(CLASS);
      cover(el, cells);
      return Promise.resolve();
    }
    return bake(el, cells);
  }

  // A blur that does not ask a canvas to do it. Safari's canvas has no filter
  // property at all: setting one throws nothing and does nothing, so a bake that
  // leaned on it came out as the picture it started as — success reported, no
  // blur anywhere. Shrinking the picture and drawing it back up throws the detail
  // away and lets the smoothing put a soft version back, which is what a blur
  // looks like, and every browser can do it. Colour is drained with a saturation
  // blend for the same reason.
  //
  // ponytail: one pass down and one back up, so a large radius comes out
  // soft-blocky rather than gaussian. That hides a person perfectly well. Halve
  // repeatedly if it ever has to match the CSS blur exactly.
  function soften(bmp, W, H, radius) {
    var f = Math.max(1, radius);
    var sw = Math.max(1, Math.round(W / f));
    var sh = Math.max(1, Math.round(H / f));
    var small = new OffscreenCanvas(sw, sh);
    var mc = small.getContext("2d");
    mc.imageSmoothingEnabled = true;
    mc.imageSmoothingQuality = "high";
    mc.drawImage(bmp, 0, 0, sw, sh);

    var out = new OffscreenCanvas(W, H);
    var oc = out.getContext("2d");
    oc.imageSmoothingEnabled = true;
    oc.imageSmoothingQuality = "high";
    oc.drawImage(small, 0, 0, W, H);
    if (GRAY) {
      oc.globalCompositeOperation = "saturation";
      oc.fillStyle = "hsl(0,0%,50%)";
      oc.fillRect(0, 0, W, H);
      oc.globalCompositeOperation = "source-over";
    }
    return out;
  }

  // Draw the full picture, blur it whole into a second canvas, then copy each
  // person's rectangle from the blurred copy back over the sharp one — copying
  // pre-blurred pixels rather than blurring each rectangle in place, so there is
  // no dark halo where a blur samples past the rectangle's edge into nothing.
  // Write the result back onto the element and keep the original for hover and
  // reset. The baked url is also the guard against re-entering detection: it is
  // what the src watcher sees when the write-back lands.
  //
  // The picture goes back in as a JPEG data url, which is what HaramBlur does.
  // The pixels travel in the attribute itself, so nothing has to stay alive
  // behind a handle and nothing has to be revoked, and a page that copies the
  // node or reads its src back gets the baked picture rather than a handle that
  // may already be gone.
  //
  // ponytail: JPEG has no transparency, so a picture with a see-through
  // background bakes onto black. Encode as PNG if a page turns up where that
  // shows — at the cost of a much longer string in the DOM.
  //
  // The radius is the setting's, which is in the pixels the picture is shown at,
  // scaled into the pixels the picture is made of. A wide photograph in a narrow
  // column is drawn down on its way to the screen and would take the blur down
  // with it.
  function dataUrl(blob) {
    return new Promise(function (resolve, reject) {
      var reader = new FileReader();
      reader.onload = function () {
        resolve(reader.result);
      };
      reader.onerror = reject;
      reader.readAsDataURL(blob);
    });
  }

  function bake(el, cells) {
    var src = el.__abBg || el.src;
    return fromUrl(src, BAKE_CAP)
      .then(function (bmp) {
        var W = bmp.width,
          H = bmp.height;
        var sharp = new OffscreenCanvas(W, H);
        var sc = sharp.getContext("2d");
        sc.drawImage(bmp, 0, 0);
        var soft = soften(bmp, W, H, (AMOUNT * W) / (el.clientWidth || W));
        for (var i = 0; i < cells.length; i++) {
          var c = cells[i];
          var sx = Math.max(0, Math.floor(c.x * W));
          var sy = Math.max(0, Math.floor(c.y * H));
          var sw = Math.min(W - sx, Math.ceil(c.w * W));
          var sh = Math.min(H - sy, Math.ceil(c.h * H));
          if (sw <= 0 || sh <= 0) continue;
          sc.drawImage(soft, sx, sy, sw, sh, sx, sy, sw, sh);
        }
        return sharp.convertToBlob({ type: "image/jpeg", quality: 0.9 });
      })
      .then(dataUrl)
      .then(function (url) {
        // Only the first bake records the original: what is on the element
        // during a re-bake is the last bake, not the page's picture.
        if (!el.__abBaked) {
          if (el.__abBg) el.__abOriginal = { bg: el.style.backgroundImage };
          else {
            el.__abOriginal = { src: el.src, srcset: el.getAttribute("srcset"), sources: null };
            // A <picture>'s <source> siblings out-rank the baked src, so detach
            // them while the bake is in place and keep them to put back on
            // reveal or reset.
            if (el.parentNode && el.parentNode.tagName === "PICTURE") {
              var srcs = [];
              var found = el.parentNode.querySelectorAll("source");
              for (var k = 0; k < found.length; k++) srcs.push(found[k]);
              for (var m = 0; m < srcs.length; m++) srcs[m].parentNode.removeChild(srcs[m]);
              el.__abOriginal.sources = srcs;
            }
          }
        }
        el.__abBaked = url;
        // Keep __abSeen in step with this write, or the src watcher would read
        // it back as a picture it has not seen and undo everything just done.
        el.__abSeen = url;
        if (el.__abBg) el.style.backgroundImage = "url(" + url + ")";
        else {
          el.removeAttribute("srcset");
          el.src = url;
        }
      })
      .catch(function (e) {
        // The picture could not be read, decoded or encoded, so it was never
        // baked. Fall back to a whole-element blur, which still does not float,
        // then hand the failure on rather than swallow it: the caller marks
        // whatever this resolves to as blurred, and a bake that never happened
        // reported as one is how a blur that did nothing went unnoticed.
        el.classList.add(CLASS);
        throw e;
      });
  }

  // Undo a bake: forget the baked picture, and put the original back only when
  // asked. A reveal restores it; a reset for a fresh picture does not, because
  // the page has already put the new one in place and painting the old one back
  // would wipe it out.
  function unbake(el, restore) {
    if (!el.__abBaked) return;
    if (restore && el.__abOriginal) {
      if (el.__abBg) {
        el.style.backgroundImage = el.__abOriginal.bg;
        el.__abSeen = background(el);
      } else {
        if (el.__abOriginal.sources && el.parentNode) {
          for (var i = 0; i < el.__abOriginal.sources.length; i++) {
            el.parentNode.insertBefore(el.__abOriginal.sources[i], el);
          }
        }
        if (el.__abOriginal.srcset) el.setAttribute("srcset", el.__abOriginal.srcset);
        if (el.__abOriginal.src != null) el.src = el.__abOriginal.src;
        else el.removeAttribute("src");
        el.__abSeen = el.src;
      }
    }
    el.__abBaked = null;
    el.__abOriginal = null;
  }

  function show(el) {
    el.classList.remove(CLASS);
    uncover(el);
    unbake(el, true);
  }

  // Lift the blur off a picture while the pointer is over it, and put it back
  // when the pointer leaves. A picture blurred by a filter on the element is
  // lifted by a class; a baked one by swapping the original picture back in,
  // since its blur is in the pixels. __abSeen is kept in step with each swap, or
  // the src watcher would read it back as a new picture and reset everything
  // this was mid-toggle. A video's patch box lives beside the video, not inside
  // it, so a CSS hover rule on the video cannot reach it — its class is toggled
  // here instead.
  function lift(el, off) {
    if (el.__abBaked && el.__abOriginal) {
      if (el.__abBg) {
        el.style.backgroundImage = off ? el.__abOriginal.bg : "url(" + el.__abBaked + ")";
        el.__abSeen = background(el);
      } else {
        el.src = off ? el.__abOriginal.src : el.__abBaked;
        el.__abSeen = el.src;
      }
      return;
    }
    el.classList.toggle(CLASS + "-off", off);
    var entry = el.__abCover;
    if (entry && entry.box) entry.box.classList.toggle(CLASS + "-off", off);
  }

  function hoverable(el) {
    el.addEventListener("mouseenter", function () {
      lift(el, true);
    });
    el.addEventListener("mouseleave", function () {
      lift(el, false);
    });
  }

  // A baked blur travels with the element it is part of, but a video's patch box
  // and the marks overlay are laid beside the element and have to be re-placed
  // when the page moves. Region blur only lays a box for video now. Hooked up by
  // run(), because until then there is nothing laid over anything.
  function watchLayout() {
    if (!REGIONS && !MARKS) return;
    window.addEventListener("resize", reflow);
    // A video starting or stopping decides whether the thumbnail under it is the
    // picture on screen. Capture, because these two do not bubble.
    document.addEventListener("play", reflow, true);
    document.addEventListener("pause", reflow, true);
  }

  function checkImage(img) {
    mark(img, "looking");
    console.log("blur: processing", img.__abBg || img.src);
    return snapshot(img)
      .then(findPeople)
      .then(function (people) {
        // No answer means the pixels could not be read — the worker got the
        // frame but could not look at it. An image we could not check stays
        // covered, same as a read that failed outright below: we do not know
        // what is in it. A failure adds no abx-blur-processed, so the hold rule
        // keeps it covered on its own; nothing to add here.
        if (!people) {
          mark(img, "failed", "detector");
          return;
        }
        if (MARKS) outline(img, people);
        var picked = people.filter(wanted);
        var note = tally(picked.length, people.length);
        if (!picked.length) {
          mark(img, "clear", note);
          return;
        }
        // Wait for the bake before letting the hold go: the hold blurs the
        // picture on its own until then, so the blurred pixels are in place the
        // moment it lifts and the original never flashes. The CSS paths resolve
        // at once, so there is no wait for them.
        return hide(img, picked).then(function () {
          mark(img, "blurred", note);
        });
      })
      .catch(function (e) {
        // Either the read failed — the anonymous re-fetch was refused, so there
        // is nothing to look at — or the bake did, in which case it has already
        // put a whole-element blur on. Kept covered either way for the same
        // reason as above: no abx-blur-processed, so the hold rule holds it.
        mark(img, "failed", "unreadable or unbakeable");
        if (MARKS) console.warn("blur: cannot bake", img.__abBg || img.src, e);
      });
  }

  // Pictures are checked one at a time, and whatever is on screen when the
  // model comes free goes next. That is the whole point of the queue: a page
  // full of avatars and thumbnails would otherwise spend its first seconds on
  // whichever ones happen to sit earliest in the HTML, while the reader is
  // looking at something else. Scrolling needs no bookkeeping — the choice is
  // made fresh each time, so wherever the page is by then is what gets served.
  //
  // ponytail: a scan of the queue per picture. Fine for the few hundred a page
  // has; if some page ever holds thousands, bucket them by screenful instead.
  var queue = [];
  var running = false;

  function onScreen(r) {
    return (
      r.bottom > 0 &&
      r.right > 0 &&
      r.top < (window.innerHeight || 0) &&
      r.left < (window.innerWidth || 0)
    );
  }

  function pump() {
    if (!live || running || !queue.length) return;
    var pick = queue.length - 1;
    for (var i = 0; i < queue.length; i++) {
      if (onScreen(queue[i].getBoundingClientRect())) {
        pick = i;
        break;
      }
    }
    // Nothing on screen: take the last one added, which on a page being
    // scrolled is the nearest to where the reader is heading.
    var img = queue.splice(pick, 1)[0];
    running = true;
    checkImage(img).then(function () {
      running = false;
      pump();
    });
  }

  // A picture joins the queue once it has loaded, so a slow one cannot hold up
  // the ones that are ready. Every picture past the size cutoff is looked at —
  // a thumbnail holds a person the same as a photograph does.
  //
  // A loaded element still reports no size when there is nothing behind it: no
  // `src` yet, an empty one a lazy loader has not filled in, or a request that
  // failed. Waiting for `load` covers all three — it fires whenever a real
  // picture arrives, however late, and simply never fires for one that is
  // broken.
  //
  // The size cutoff itself is judged by the box the picture is drawn in —
  // clientWidth/clientHeight — not by the pixels of whatever file happens to be
  // loaded. A lazy-loaded `<picture>` reports `load` for a tiny placeholder
  // before the real photo lands, so its naturalWidth is briefly 1x1 while its
  // layout box is already full size; sizing off naturalWidth read that
  // placeholder as "too small" and skipped the picture for good, since a skip
  // is a verdict and nothing rechecks it. The box a picture is laid out in
  // doesn't have that problem — a background has no pixels of its own to read
  // in the first place, so it was always measured this way; images are now
  // measured the same way for the same reason.
  function enqueue(img) {
    if (queue.indexOf(img) >= 0) return;
    if (img.__abBg) {
      if (tooSmall(img.clientWidth, img.clientHeight)) {
        mark(img, "skipped", img.clientWidth + "x" + img.clientHeight + ", too small");
        return;
      }
      queue.push(img);
      pump();
      return;
    }
    if (!img.complete || !img.naturalWidth) {
      img.addEventListener("load", function () { enqueue(img); }, { once: true });
      return;
    }
    if (tooSmall(img.clientWidth, img.clientHeight)) {
      mark(img, "skipped", img.clientWidth + "x" + img.clientHeight + ", too small");
      return;
    }
    queue.push(img);
    pump();
  }

  var visible = new IntersectionObserver(
    function (entries) {
      for (var i = 0; i < entries.length; i++) {
        if (!entries[i].isIntersecting) continue;
        visible.unobserve(entries[i].target);
        enqueue(entries[i].target);
      }
    },
    { rootMargin: "200px" }
  );

  // Everything held on an element is about the picture it was showing. When the
  // page puts a different one on it, all of that goes: the patches, the
  // outlines, the size the last frame went over at, and its place in the queue.
  // The same clean-out a video gets on `emptied`, for the pictures that have no
  // such event.
  function reset(el) {
    el.classList.remove(CLASS);
    // Drop the bake without repainting: the page has already put the new picture
    // on this node, so painting the old original back would wipe it out.
    unbake(el, false);
    // Re-arm the hold: the next picture on this node has not been looked at, so
    // drop the last one's verdict class or the hold rule would skip it.
    el.classList.remove(CLASS + "-processed");
    if (MARKS) outline(el, []);
    visible.unobserve(el);
    var at = queue.indexOf(el);
    if (at >= 0) queue.splice(at, 1);
    el.__abStill = null;
  }

  // Which picture this element was looked at for, not whether it was looked at:
  // a page that reuses a node for a second picture — a carousel, or a feed being
  // scrolled back to — otherwise keeps the first picture's verdict and leaves
  // the first picture's patches and outlines sitting over the second one.
  //
  // An element with no picture on it yet is remembered as such and left alone.
  // A lazy loader filling its src in comes back through here.
  function watchImage(img, src) {
    // __abSeen is not just "the first src found" — every write this script
    // itself makes (bake, unbake, the hover swap) keeps it in sync with
    // whatever it just put there, so it always names the current expected
    // picture rather than the original one. A src equal to it is one of those
    // writes landing, whatever value it happens to carry. Anything else is the
    // page changing the picture — including a page that puts the original back
    // itself, undoing a bake behind this script's back, which looks exactly
    // like a new picture arriving and is handled the same way: reset and
    // requeued for a fresh verdict.
    if (img.__abSeen === src) return;
    var again = img.__abSeen !== undefined;
    img.__abSeen = src;
    if (again) reset(img);
    else if (HOVER_IMAGES) hoverable(img);
    if (!src) return;
    mark(img, "queued");
    visible.observe(img);
  }

  // A video is blurred whole by a filter on the element, turned on while a person
  // is on screen and off when the run comes back clear. It is sampled over and
  // over; the answer lags the picture by however long a sample takes, which is
  // why a run of frames has to agree before the blur goes on or comes off.
  function watchVideo(v) {
    if (v.__abSeen) return;
    v.__abSeen = true;
    if (HOVER_VIDEOS) hoverable(v);
    mark(v, "queued");

    var busy = false;
    var found = 0;
    var clear = 0;
    var fails = 0;
    var last = 0;

    // A frame that could not be read. The run has to be counted rather than
    // acted on, because a video is sampled over and over and one bad frame says
    // nothing about the next.
    //
    // Giving up takes the cover off, because a video nobody is sampling any
    // more is a video whose patches can no longer move, and a patch that cannot
    // move is a smear parked over a picture that keeps going. The one exception
    // is a frame that could not be read at all: there, revealing the picture
    // could mean showing exactly what the cover was hiding, so it stays on.
    function failed(why) {
      if (++fails >= FAIL_RUN) {
        v.__abStop = true;
        // A frame the model could not read says nothing, so it is never a
        // reason to blur — except a frame that could not be read at all. That
        // one is left covered rather than revealed, because unreadable is the
        // one failure where showing the picture could be showing exactly what
        // the cover was there to hide.
        if (why !== "unreadable") {
          show(v);
          v.classList.add(CLASS + "-processed");
        }
        if (MARKS) outline(v, []);
      }
      mark(v, "failed", why);
    }

    // Changing crossorigin on a loaded media element does nothing until the
    // next load, so a tainted video is reloaded once, in place, keeping its
    // position and play state. A blob: source is MSE and must never be
    // reloaded — it is also never tainted, so it never gets here. The source is
    // read off currentSrc, because a player that sets it with a <source> child
    // leaves src empty.
    //
    // If the reload then fails, the server would not allow the read after all,
    // and the element is put back exactly as the page had it and loaded again.
    // A video that cannot be looked at still has to play.
    function retry() {
      var src = v.currentSrc || v.src;
      if (v.__abRetried || !src || src.lastIndexOf("blob:", 0) === 0) return false;
      v.__abRetried = true;
      var at = v.currentTime;
      var playing = !v.paused;
      function resume() {
        try {
          v.currentTime = at;
        } catch (e) {}
        if (playing) {
          var p = v.play();
          if (p && p.catch) p.catch(function () {});
        }
      }
      v.addEventListener("loadedmetadata", resume, { once: true });
      v.addEventListener(
        "error",
        function () {
          // Not a hiccup: the server has refused the read outright, and asking
          // again would get the same answer. Given up on at once.
          fails = FAIL_RUN - 1;
          failed("not allowed to read");
          v.removeAttribute("crossorigin");
          v.addEventListener("loadedmetadata", resume, { once: true });
          v.load();
        },
        { once: true }
      );
      v.crossOrigin = "anonymous";
      v.load();
      return true;
    }

    function look() {
      if (!live || v.__abStop || v.__abOff || busy) return;
      // A video's own size only turns up once it has metadata, so this is
      // checked here rather than when it was first seen.
      if (v.videoWidth && tooSmall(v.videoWidth, v.videoHeight)) {
        v.__abStop = true;
        mark(v, "skipped", v.videoWidth + "x" + v.videoHeight + ", too small");
        return;
      }
      busy = true;
      if (!v.__abLooked) mark(v, "looking");
      var at = v.currentTime;
      snapshot(v)
        .then(findPeople)
        .then(function (people) {
          v.__abLooked = true;
          // No answer is nearly always a frame the page was not allowed to
          // read, which the model reports as a failure rather than throwing —
          // so the reload that asks for permission has to be tried from here
          // too, not only from the catch below.
          if (!people) {
            if (retry()) return;
            failed("detector");
            return;
          }
          // An answer arrived, so whatever came before it was a hiccup.
          fails = 0;
          // The video ran on while the model was thinking. Covering where
          // somebody was half a second ago is worse than covering nothing, so
          // the answer is dropped and the hold stays on until a fresh one
          // lands. HaramBlur's own cutoff, measured both ways: a seek backwards
          // leaves the answer just as far from the picture on screen.
          if (Math.abs(v.currentTime - at) > STALE_S) {
            // The outlines say what the runtime is working from, and it is
            // working from nothing here. Left up, they are the boxes of a frame
            // that has been passed, sitting still over a moving picture.
            if (MARKS) outline(v, []);
            return;
          }
          if (MARKS) outline(v, people);
          var picked = people.filter(wanted);
          if (!picked.length) {
            found = 0;
            if (++clear >= CLEAR_RUN) show(v);
          } else {
            clear = 0;
            if (++found >= BLUR_RUN) hide(v, picked);
          }
          // What the cover is doing right now, not what this one frame said:
          // inside a run neither has taken effect yet.
          var still = v.classList.contains(CLASS);
          mark(v, still ? "blurred" : "clear", tally(picked.length, people.length));
        })
        .then(function () {
          busy = false;
        })
        .catch(function (e) {
          busy = false;
          if (retry()) return;
          failed("unreadable");
          if (MARKS) console.warn("blur: cannot read video", v.currentSrc || v.src, e);
        });
    }

    // The loop is kept turning while the blur is stopped rather than torn down:
    // it costs a callback on a frame the browser was drawing anyway, and it is
    // what lets a video that was already playing carry on being sampled the
    // moment the blur is run again — nothing has to find it a second time.
    function sample(now) {
      if (v.__abStop || v.__abOff) return;
      if (live && !v.paused && !v.ended && !busy && v.readyState >= 2 && now - last >= SAMPLE_MS) {
        last = now;
        look();
      }
      schedule();
    }

    // requestVideoFrameCallback fires once per presented frame, so a paused or
    // stalled video costs nothing and no frame is ever sampled twice.
    function schedule() {
      if (v.__abStop || v.__abOff || v.paused || v.ended) return;
      if (v.requestVideoFrameCallback) v.requestVideoFrameCallback(sample);
      else requestAnimationFrame(sample);
    }

    // The reader's own switch, one per video, in the top-right corner. Off, no
    // frame is sent to the model — every sampling path checks v.__abOff — and
    // the cover comes off. On again, the current frame is looked at at once and
    // the sampling loop is restarted.
    var btn = null;
    function place() {
      // The button is an absolute sibling of the video, so it shares the video's
      // containing block and rides along on scroll. Its own corner is measured
      // at the origin and the video's top-right is stepped to from there, which
      // holds however the page nests the video — the same trick the patch layers
      // use.
      btn.style.left = "0px";
      btn.style.top = "0px";
      var o = btn.getBoundingClientRect();
      var r = v.getBoundingClientRect();
      btn.style.left = r.right - o.left - btn.offsetWidth - 6 + "px";
      btn.style.top = r.top - o.top + 6 + "px";
    }
    function toggle() {
      v.__abOff = !v.__abOff;
      btn.textContent = v.__abOff ? "Blur off" : "Blur on";
      if (v.__abOff) {
        // Reader turned this video's blur off: reveal it and mark it processed
        // so the hold rule lets go too.
        v.classList.add(CLASS + "-processed");
        show(v);
        if (MARKS) outline(v, []);
      } else {
        // Back on: re-arm the hold, then look again from scratch.
        v.classList.remove(CLASS + "-processed");
        look();
        schedule();
      }
    }
    function addToggle() {
      // Nothing on an icon or a control-strip video: the button would be bigger
      // than the picture. Measured by how big it is drawn, not its own pixels.
      if (btn || tooSmall(v.clientWidth, v.clientHeight)) return;
      btn = document.createElement("button");
      btn.type = "button";
      btn.className = CLASS + "-toggle";
      btn.textContent = "Blur on";
      btn.addEventListener("click", function (e) {
        e.preventDefault();
        e.stopPropagation();
        toggle();
      });
      (v.parentNode || document.body).insertBefore(btn, v);
      place();
      if (window.ResizeObserver) new ResizeObserver(place).observe(v);
    }

    // The reader's switch is put up as soon as the video has laid out, before
    // any play. Nothing is looked at here: a video is read only while it plays,
    // the same as HaramBlur, so its poster and parked first frame are left
    // alone.
    function ready() {
      if (v.__abStop) return;
      addToggle();
    }

    v.addEventListener("loadeddata", ready);
    v.addEventListener("loadedmetadata", ready);
    ready();

    // A player that moves to the next video keeps the element and swaps what is
    // in it. Everything held on it is about the video that has gone: the
    // patches, the outlines, how many frames in a row said the same thing, and
    // whether this one was given up on. All of it goes with the source, and the
    // new video is looked at from scratch.
    v.addEventListener("emptied", function () {
      found = 0;
      clear = 0;
      fails = 0;
      v.__abStop = false;
      show(v);
      // Re-arm the hold for the new video, same as reset() does for a picture.
      v.classList.remove(CLASS + "-processed");
      if (MARKS) outline(v, []);
    });

    v.addEventListener("play", schedule);
    schedule();
  }

  // Images and videos are two switches, as they are in HaramBlur: with one off
  // that kind of media is never looked at and never touched.
  function watch(el) {
    if (el.tagName === "IMG") {
      if (IMAGES) watchImage(el, el.src);
    } else if (VIDEOS) {
      watchVideo(el);
    }
  }

  function sweep(root) {
    if (!root || root.nodeType !== 1) return;
    if (root.tagName === "IMG" || root.tagName === "VIDEO") watch(root);
    if (!root.querySelectorAll) return;
    var found = root.querySelectorAll("img,video");
    for (var i = 0; i < found.length; i++) watch(found[i]);
  }

  // A picture is not always an <img>. A page can paint one on any element as a
  // CSS background, and a player's own thumbnail usually is one: YouTube parks a
  // bare <div> over the video with the still on it, and nothing about that
  // element says it is a picture. So there is no selector to write — every
  // element is asked what its background is, which is what HaramBlur does and
  // the only way that thumbnail is ever found.
  //
  // Asking costs a style resolution each, so an element is asked once and marked
  // with an attribute, and the next sweep selects on not having the mark. The
  // sweep itself runs at most once every SCAN_MS: a page that inserts a hundred
  // nodes is one page to look over, not a hundred.
  //
  // A background swapped on an element already marked is caught by the style
  // watcher below instead of a sweep — see watchBackground.
  var URL_IN = /url\(\s*['"]?(.*?)['"]?\s*\)/i;
  var scanning = 0;

  function background(el) {
    var v = window.getComputedStyle(el).backgroundImage;
    if (!v || v === "none") return "";
    var m = URL_IN.exec(v);
    return m ? m[1] : "";
  }

  // Pictures and videos are left out: those have a picture of their own and are
  // already watched as one. So are the page and its body, because a background
  // on either is behind everything else on the page, and holding one of those
  // back holds back the whole page rather than a picture in it.
  var UNSEEN =
    "*:not(img):not(video):not(html):not(body):not([data-ab-bg])";

  // The on-load stylesheet blurs anything with an inline background url,
  // because before this sweep has run there is nothing else to match a
  // background div on. Once every element has been looked at, the ones holding a
  // picture carry data-ab-hold and that blanket rule only blurs elements no
  // verdict will ever clear — a box with a border image, a gradient placeholder
  // — so it is cut out of the sheet. Written exactly as blur_preload_css joins
  // it, leading comma and all.
  var BLANKET = ',[style*="background-image: url("]';
  var unblanketed = 0;

  function scan() {
    scanning = 0;
    // A sweep already booked when the blur was stopped still comes due. It is
    // dropped rather than run: run() books a fresh one.
    if (!live) return;
    var all = document.querySelectorAll(UNSEEN);
    for (var i = 0; i < all.length; i++) {
      var url = background(all[i]);
      all[i].dataset.abBg = "";
      if (!url) continue;
      // The tag hold rule reaches img and video on its own; a background div has
      // no tag to match, so mark it here — the same moment the old per-element
      // hold went on. The hold rule covers it until a verdict clears the mark.
      all[i].dataset.abHold = "";
      all[i].__abBg = url;
      watchImage(all[i], url);
    }
    if (!unblanketed) {
      unblanketed = 1;
      var pre = document.querySelector("style[data-ab-css]");
      if (pre) pre.textContent = pre.textContent.replace(BLANKET, "");
    }
  }

  function sweepBackgrounds() {
    if (!IMAGES || !live || scanning) return;
    scanning = setTimeout(scan, SCAN_MS);
  }

  // A background reasserted, on an element already found to carry one. A
  // single-page site that re-renders its own inline style puts the original,
  // unblurred url straight back over a baked one — the same page behaviour the
  // src watcher below exists for, just on a different attribute. watchImage
  // already tells its own write from the page's, so routing through it here
  // recovers the same way an <img> src change does: no abx-blur-processed, so
  // the hold covers it again, and it goes back through detection from there.
  // __abBg is kept live here for the same reason el.src needs no such update —
  // bake() reads pixels from it, and a stale one would bake the wrong picture.
  // Not touched when the write was this script's own baked copy landing: that
  // is not a picture to remember as the source, only the __abSeen check below
  // needs to see it. Elements never found to carry a background are skipped
  // rather than resolving their style on every write — most style attributes on
  // a page change for reasons that have nothing to do with a picture.
  function watchBackground(el) {
    if (!el.__abBg) return;
    var url = background(el);
    if (el.__abBaked && url === el.__abBaked) return;
    el.__abBg = url;
    watchImage(el, url);
  }

  // Nodes arriving, and the source of a picture already here changing. The
  // second is what a single-page site does instead of loading a page: the feed
  // and the player are the same elements throughout, and going back puts
  // different pictures in them. HaramBlur watches the same one attribute for the
  // same reason; a CSS background gets the same treatment on "style" instead.
  var watcher = new MutationObserver(function (records) {
    for (var i = 0; i < records.length; i++) {
      var record = records[i];
      if (record.type === "attributes") {
        if (record.attributeName === "style") watchBackground(record.target);
        else watch(record.target);
        continue;
      }
      sweepBackgrounds();
      // A player taken out of the page rather than stopped pauses on its way
      // out, and that pause fires on a node no longer in the page, where the
      // listener on the document never hears it. So the picture it was over
      // learns it is a picture again from here. Placing is one pass over the
      // layers, once a frame at most, and does nothing when there are none.
      if (record.removedNodes.length) reflow();
      var added = record.addedNodes;
      for (var j = 0; j < added.length; j++) sweep(added[j]);
    }
  });

  function sweepAll() {
    sweep(document.body);
    sweepBackgrounds();
  }

  // Everything above this only declares things: no stylesheet in the page, no
  // listener, no observer, no frame at the model. This is what starts it.
  //
  // Run again after a stop, it sweeps the page as it now stands, so pictures
  // that arrived while it was stopped are picked up along with the rest.
  function run() {
    if (live) return;
    live = true;
    if (!wired) {
      wired = true;
      (document.head || document.documentElement).appendChild(sheet);
      watchLayout();
    }
    watcher.observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src", "style"],
    });
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", sweepAll, { once: true });
    } else {
      sweepAll();
    }
    // A queue left part-done by a stop carries on from where it was.
    pump();
  }

  // Stop looking: no new picture is queued, no frame is sent to the model. What
  // is already blurred stays blurred and the hold stays on — this is a switch,
  // not an undo, and lifting a blur nobody has re-checked would show exactly
  // what it was put there to cover.
  function stop() {
    live = false;
    watcher.disconnect();
  }

  // With the debugging panel wrapped around this file, CONTROL is its way in:
  // it takes the handle and decides when the blur runs. On its own — no panel,
  // no CONTROL — the runtime starts itself, which is the normal page.
  if (typeof CONTROL === "function") {
    CONTROL({
      run: run,
      stop: stop,
      lay: lay,
      CLASS: CLASS,
      // The video sample rate is read live by sample(), so setting it here
      // changes the rate on the very next frame.
      rate: function (ms) {
        if (ms) SAMPLE_MS = ms;
        return SAMPLE_MS;
      },
      // What the panel prints as the settings this page was given.
      config: {
        amount: AMOUNT,
        strictness: STRICTNESS,
        score: SCORE,
        men: MEN,
        women: WOMEN,
        images: IMAGES,
        videos: VIDEOS,
        regions: REGIONS,
        gray: GRAY,
        onLoad: ON_LOAD,
        hoverImages: HOVER_IMAGES,
        hoverVideos: HOVER_VIDEOS,
        modelSize: MODEL_SIZE,
        imageCap: IMAGE_CAP,
        videoCap: VIDEO_CAP,
        minSize: MIN_SIZE,
      },
    });
  } else {
    run();
  }
})();
