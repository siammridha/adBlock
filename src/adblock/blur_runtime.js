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
// Several models, one at a time. Each entry in MODELS is a whole worker: it
// loads whatever library it needs and answers the same question — hand it a
// frame, get back a list of boxes with what the model made of each one. Nothing
// outside the table knows which model is running. Adding another is adding a
// row.
//
// They do not all see the same thing. A face model puts a box round a head, so
// the box has to be grown down over the body; a person model already returns the
// whole person and its box is used as it stands. A model that reads man or woman
// fills that in; one that only finds people says "person" and both switches
// treat it as a match. Each row says which it is.
//
// With the panel on, the model can be changed from it, for this page, without
// reloading. That is for trying one against another on a page that matters; the
// lasting choice is a setting.
//
// __BLUR_AMOUNT__, __BLUR_STRICTNESS__, __BLUR_MEN__, __BLUR_WOMEN__,
// __BLUR_VIDEOS__, __BLUR_REGIONS__, __BLUR_MARKS__, __BLUR_RESIZE__,
// __BLUR_IMG_SIZE__, __BLUR_VIDEO_SIZE__ and __BLUR_MODEL__ are replaced with
// Adblock's settings before this is injected.
(function () {
  var AMOUNT = __BLUR_AMOUNT__;
  // Strictness runs 0–100 and slides the bars a face has to clear before it
  // counts as a man or a woman. Higher strictness, lower bars, more blurring.
  // At 50 the bars are HaramBlur's own: a man at 0.30, a woman at 0.25, and a
  // face read as a woman the model is under 0.20 sure of counted as a man
  // instead. HaramBlur also skips anyone it reads as under 20.
  var STRICTNESS = __BLUR_STRICTNESS__;
  var SHIFT = ((50 - STRICTNESS) / 100) * 0.5;
  var MALE_MIN = 0.3 + SHIFT;
  var FEMALE_MIN = 0.25 + SHIFT;
  var MALE_MAX = 0.2 - SHIFT; // a weak "female" reads as a man below this
  var MIN_AGE = 20;
  var MEN = __BLUR_MEN__;
  var WOMEN = __BLUR_WOMEN__;
  var VIDEOS = __BLUR_VIDEOS__;
  var REGIONS = __BLUR_REGIONS__;
  var MARKS = __BLUR_MARKS__;

  var CLASS = "abx-blur";
  var HUD = "abx-blur-hud"; // the corner panel, only ever built when MARKS is on
  // A picture is shrunk until its longest side fits, keeping its shape. Bigger
  // finds smaller faces and costs more time. Off, it goes through at its own
  // size. The defaults are the sizes HaramBlur uses.
  var RESIZE = __BLUR_RESIZE__;
  var IMG_MAX = __BLUR_IMG_SIZE__;
  var VID_MAX = __BLUR_VIDEO_SIZE__;
  // A picture with a side under this many of its own pixels is not looked at.
  // Icons, bullets, spacers and tracking pixels are most of what a page carries
  // and none of them can hold a face, but each one costs a run of the detector.
  var SKIP_SMALL = __BLUR_SKIP_SMALL__;
  var MIN_SIZE = __BLUR_MIN_SIZE__;
  var SAMPLE_MS = 200; // a video is sampled at most this often
  var FRAME_EVERY = 5; // the panel redraws a video every this many samples
  var CLEAR_RUN = 3; // clean frames in a row before a video unblurs again
  var MAX_FACES = 20; // however many people are in the picture

  if (!window.createImageBitmap || !window.Worker) return;

  // The library and its weights, at whatever version is current. Roughly 1.5 MB
  // of library, 0.5 MB to find the faces and 6.5 MB to tell them apart,
  // downloaded on the first page that has a picture worth looking at. The
  // browser caches all of it after that.
  //
  // The path to the WebAssembly build of the maths is not set here. The library
  // fills it in itself, pointing at the exact version of the maths it was built
  // against, so it stays right as the library moves. Overriding it with a
  // version of our own would break the moment those two parted ways.
  //
  // ponytail: fetched from a CDN, unpinned. Adblock already downloads its own
  // remote resources; move these in there and serve them from the admin
  // endpoint if depending on someone else's host at page load, or on whatever
  // they published today, turns out to be a problem.
  var HUMAN = "https://cdn.jsdelivr.net/npm/@vladmandic/human/dist/human.js";
  var HUMAN_MODELS = "https://cdn.jsdelivr.net/npm/@vladmandic/human/models/";

  // No document in a worker, so a frame is drawn on an OffscreenCanvas and read
  // back as pixels. The draw is a resize the GPU already had to do; nothing is
  // encoded, decoded or copied across a thread.
  //
  // Everything the library can do but face detection and face description is
  // switched off: no mesh, no iris, no expression, no body, no hands. The
  // description model is what answers man or woman, and it is the large half of
  // the download.
  //
  // WebAssembly, always. The library will also run on WebGPU, WebGL or plain
  // JavaScript, and left to choose it lands on plain JavaScript: 1.5 seconds a
  // picture against 65 milliseconds. That is not a slower option, it is a
  // broken one. Asking for a backend the browser cannot give does not fail
  // either — the library drops to plain JavaScript and reports itself ready —
  // and a failed attempt leaves the maths library in a state where a second,
  // working backend no longer takes. So one backend is asked for, the one that
  // is always there, and the panel says which one was reached.
  //
  // ponytail: WebGPU would be faster again where a browser really has it, but
  // finding that out safely means asking for a graphics adapter before booting.
  // Worth doing only if 65 milliseconds ever turns out to be too slow.

  // The models that read man or woman are ONNX, off the Hugging Face hub, run
  // by transformers.js. It is an ES module, so a worker reaches it with a
  // dynamic import rather than importScripts, and it finds its own WebAssembly
  // the same way the Human library does — pointed at the version it was built
  // against, so neither has to be pinned here.
  //
  // Not the `.web.js` build, which is meant to be fed through a bundler: it
  // imports `onnxruntime-web/webgpu` by bare name, and a browser has no way to
  // resolve that. `transformers.min.js` is the same library with everything
  // already resolved, and is the one a page can import directly.
  var TJS = "https://cdn.jsdelivr.net/npm/@huggingface/transformers/dist/transformers.min.js";

  // Which crop a gender model wants is not a detail — it is the model. One
  // trained on faces reads a face and nothing else; hand it a whole standing
  // person and it is being asked a question it was never taught. One trained on
  // pedestrians is the other way round. So a classifier is written down with the
  // kind of box it takes, and a model row may only pair it with a detector that
  // produces that kind.
  //
  // `pad` opens the box out before cropping. Face sets are cut with the hair and
  // the chin in frame, and a detector's box is tighter than that, so a face crop
  // is widened to match what the model was trained on. A body crop is already
  // the whole person and is taken as it is.
  //
  // `opts` is passed to the pipeline as written. Most of the hub is laid out the
  // way transformers.js expects and needs only a weight size; a model published
  // for plain transformers has its file somewhere else and has to say so.
  var GENDER = {
    fairface: {
      id: "onnx-community/fairface_gender_image_detection-ONNX",
      opts: "{ dtype: 'q8' }",
      takes: "face",
      pad: 0.35,
      size: 87,
      note: "FairFace, about 93% on a face it can see",
    },
    rizvandwiki: {
      id: "onnx-community/gender-classification-ONNX",
      opts: "{ dtype: 'q8' }",
      takes: "face",
      pad: 0.35,
      size: 87,
      note: "rizvandwiki, about 92% on a face it can see",
    },
    peta: {
      id: "NTQAI/pedestrian_gender_recognition",
      // Published for plain transformers, so the weights sit at the top of the
      // repository rather than under onnx/, and there is no quantised copy.
      opts: "{ dtype: 'fp32', model_file_name: 'model', subfolder: '' }",
      takes: "body",
      pad: 0,
      // Its preprocessor config predates the shape transformers.js reads, so the
      // library resizes the crop keeping its proportions and hands a 141x197
      // picture to a model with a fixed 224x224 grid, which fails on every call
      // whose crop was not already square. Sized here instead, squarely.
      fit: 224,
      size: 365,
      note: "PETA, about 91% on a whole person, face or no face",
    },
  };

  // The half of a worker that reads man or woman off a crop. Shared by both
  // detector families, because the question and the answer are the same either
  // way: give it the frame and a box, get back a label and how sure it is.
  //
  // A classifier of null leaves `tell` null and `gender()` answering nothing,
  // which is how a detector that already knows the answer opts out.
  function genderSrc(classifier) {
    var c = classifier ? GENDER[classifier] : null;
    return [
      "var T = null, tell = null;",
      "var tjs = " + (c ? "import('" + TJS + "')" : "Promise.resolve(null)") + ";",
      "var told = tjs.then(function (mod) {",
      "  if (!mod) return null;",
      "  T = mod;",
      // Nothing is on this machine, so do not look: without this the library
      // tries a local path first and every load waits on a 404.
      "  T.env.allowLocalModels = false;",
      "  return T.pipeline('image-classification', '" + (c ? c.id : "") + "', " +
        (c ? c.opts : "{}") + ").then(function (p) { tell = p; });",
      "});",
      // The crop is taken from the frame the detector was given, at that frame's
      // size, so the box arrives as fractions and is turned back into pixels
      // here. Clamped, because an opened-out box runs off the edge of a picture
      // whenever somebody stands near one.
      "function gender(img, b) {",
      "  if (!tell) return Promise.resolve(null);",
      "  var pad = " + (c ? c.pad : 0) + ";",
      "  var w = b[2] - b[0], h = b[3] - b[1];",
      "  var x0 = Math.max(0, Math.floor((b[0] - w * pad) * img.width));",
      "  var y0 = Math.max(0, Math.floor((b[1] - h * pad) * img.height));",
      "  var x1 = Math.min(img.width, Math.ceil((b[2] + w * pad) * img.width));",
      "  var y1 = Math.min(img.height, Math.ceil((b[3] + h * pad) * img.height));",
      // A crop of a few pixels carries no face and no person, and a model run on
      // one costs the same as a model run on a real one.
      "  if (x1 - x0 < 16 || y1 - y0 < 16) return Promise.resolve(null);",
      // crop() is async and hands back a promise, not a picture. Passing that
      // promise straight to the model is not an error it reports as one — it
      // says the input is unsupported, every call, and every face comes back
      // unclassified while the detector looks like it is working.
      "  return Promise.resolve(img.crop([x0, y0, x1, y1])).then(function (c) {",
      "    return " + (c && c.fit ? "c.resize(" + c.fit + ", " + c.fit + ")" : "c") + ";",
      "  }).then(function (c) {",
      "    return tell(c);",
      "  }).then(function (r) {",
      "    var top = r && r[0];",
      "    if (!top) return null;",
      // Every one of these answers Male/Female in some casing. Anything that
      // does not is not a gender model, and saying so beats guessing.
      "    var label = String(top.label).toLowerCase();",
      "    if (label.indexOf('female') === 0) return { gender: 'female', score: top.score };",
      "    if (label.indexOf('male') === 0) return { gender: 'male', score: top.score };",
      "    return null;",
      "  }, function () { return null; });",
      "}",
    ].join("\n");
  }

  // Human finds the faces. Its own description model reads man or woman and an
  // age off each one, and that is what runs when no classifier is named — the
  // cheap option, and the only one here that knows an age at all. Name a
  // classifier and the faces are still Human's but the answer is the other
  // model's, taken from a crop of each face.
  function humanSrc(classifier) {
    var better = !!classifier;
    return [
      genderSrc(classifier),
      "importScripts('" + HUMAN + "');",
      "var H = self.Human.Human || self.Human.default || self.Human;",
      "var canvas = new OffscreenCanvas(1, 1);",
      "var ctx = canvas.getContext('2d', { willReadFrequently: true });",
      "var human = new H({",
      "    backend: 'wasm',",
      "    modelBasePath: '" + HUMAN_MODELS + "',",
      // No frame cache. The library will answer a frame close enough to the last
      // one with the last answer instead of looking again, and "close enough"
      // covers a person walking slowly across a shot — the verdict then sticks
      // for up to two and a half seconds while the picture changes under it,
      // which reads as the video not being detected at all. There is nothing to
      // save either: a paused video presents no frames, so it is already costing
      // nothing.
      "    cacheSensitivity: 0, warmup: 'none', debug: false,",
      "    filter: { enabled: false }, gesture: { enabled: false },",
      "    body: { enabled: false }, hand: { enabled: false }, object: { enabled: false },",
      "    face: { enabled: true,",
      // square pads the frame out to a square instead of stretching it to one, so
      // a wide or tall picture reaches the detector with its shape intact.
      "      detector: { modelPath: 'blazeface.json', maxDetected: " + MAX_FACES + ",",
      "        minConfidence: 0.25, rotation: false, square: true },",
      // With a better classifier coming, the description model is dead weight —
      // it is also the large half of Human's download, so it is left out.
      "      description: { enabled: " + (better ? "false" : "true") +
        ", modelPath: 'faceres.json' },",
      "      mesh: { enabled: false }, iris: { enabled: false }, emotion: { enabled: false },",
      "      antispoof: { enabled: false }, liveness: { enabled: false } },",
      "});",
      "var ready = Promise.all([human.load(), told])",
      "  .then(",
      "    function () { postMessage({ ready: true, backend: human.tf.getBackend() }); },",
      "    function (e) { postMessage({ ready: false, why: String((e && e.message) || e) });",
      "      throw e; });",
      "onmessage = function (e) {",
      "  var id = e.data.id;",
      // The frame arrives at the size it should be read at, so the canvas takes
      // its shape rather than the other way round. Its size is read before the
      // close, which zeroes it.
      "  var bmp = e.data.bmp, w = bmp.width, h = bmp.height;",
      "  var pixels = null;",
      "  ready.then(function () {",
      "    canvas.width = w; canvas.height = h;",
      "    ctx.drawImage(bmp, 0, 0);",
      "    bmp.close();",
      "    pixels = ctx.getImageData(0, 0, w, h);",
      "    return human.detect(pixels);",
      "  }).then(function (r) {",
      "    var img = T ? new T.RawImage(pixels.data, w, h, 4) : null;",
      "    return Promise.all(r.face.map(function (f) {",
      "      var b = f.box;",
      "      var box = [b[0] / w, b[1] / h, (b[0] + b[2]) / w, (b[1] + b[3]) / h];",
      "      return (img ? gender(img, box) : Promise.resolve(null)).then(function (g) {",
      "        return {",
      "          gender: g ? g.gender : f.gender,",
      "          score: g ? g.score : f.genderScore || 0,",
      // Only Human's own description model reads an age. Without it there is no
      // age, and null is how the page is told to leave that gate alone rather
      // than treat everyone as a newborn.
      "          age: " + (better ? "null" : "f.age || 0") + ",",
      "          face: f.faceScore || f.score || 0,",
      "          box: box,",
      "        };",
      "      });",
      "    }));",
      "  }).then(function (out) {",
      "    postMessage({ id: id, out: out });",
      "  }).catch(function (err) {",
      // A frame the page is not allowed to read poisons the canvas it was drawn
      // on, and every frame after it fails the same way on the same canvas — one
      // cross-origin video would take every picture on the page down with it. So
      // the canvas is thrown away on any failure rather than reused.
      "    canvas = new OffscreenCanvas(1, 1);",
      "    ctx = canvas.getContext('2d', { willReadFrequently: true });",
      "    postMessage({ id: id, out: null, why: String((err && err.message) || err) });",
      "  });",
      "};",
    ].join("\n");
  }

  // A person detector followed by a gender model reading each person it found.
  // The detector returns the whole person, so nothing has to be guessed about
  // where the body went, and the gender model sees a standing figure rather than
  // a face — which is the point: it still answers when the face is turned away,
  // too small, or not in frame at all.
  function personSrc(detector, classifier) {
    return [
      genderSrc(classifier),
      "var find = null;",
      "var ready = told.then(function () {",
      "  return T.pipeline('object-detection', '" + detector + "', { dtype: 'q8' });",
      "}).then(function (p) {",
      "  find = p;",
      "  postMessage({ ready: true, backend: 'onnx' });",
      "}, function (e) {",
      "  postMessage({ ready: false, why: String((e && e.message) || e) });",
      "  throw e;",
      "});",
      "var canvas = new OffscreenCanvas(1, 1);",
      "var ctx = canvas.getContext('2d', { willReadFrequently: true });",
      "onmessage = function (e) {",
      "  var id = e.data.id;",
      "  var bmp = e.data.bmp, w = bmp.width, h = bmp.height;",
      "  ready.then(function () {",
      "    canvas.width = w; canvas.height = h;",
      "    ctx.drawImage(bmp, 0, 0);",
      "    bmp.close();",
      "    var d = ctx.getImageData(0, 0, w, h);",
      "    var img = new T.RawImage(d.data, w, h, 4);",
      // percentage gives the boxes as fractions of the picture, which is what
      // the page wants anyway, so nothing has to be divided back down.
      "    return find(img, { threshold: 0.5, percentage: true }).then(function (r) {",
      "      var people = r.filter(function (o) { return o.label === 'person'; });",
      "      return Promise.all(people.map(function (o) {",
      "        var box = [o.box.xmin, o.box.ymin, o.box.xmax, o.box.ymax];",
      "        return gender(img, box).then(function (g) {",
      // A person the gender model would not answer for is reported as a person
      // rather than dropped. Something is there; what is not known.
      "          return {",
      "            gender: g ? g.gender : 'person',",
      "            score: g ? g.score : o.score,",
      "            age: null,",
      "            face: o.score,",
      "            box: box,",
      "          };",
      "        });",
      "      }));",
      "    });",
      "  }).then(function (out) {",
      "    postMessage({ id: id, out: out });",
      "  }).catch(function (err) {",
      "    canvas = new OffscreenCanvas(1, 1);",
      "    ctx = canvas.getContext('2d', { willReadFrequently: true });",
      "    postMessage({ id: id, out: null, why: String((err && err.message) || err) });",
      "  });",
      "};",
    ].join("\n");
  }

  // The models on offer. Every one of them answers man or woman — that is the
  // whole question here, and a model that only finds people cannot be used to
  // hide one of the two.
  //
  // They differ in what they need to see. The first two read a face, so they
  // answer nothing about somebody photographed from behind, at a distance, or in
  // profile. The last reads a whole standing figure and does not need the face
  // at all, which is also why its box is the person and needs no growing.
  //
  // `grow` says the boxes are heads and have to be opened out over the body;
  // without it a box is already the whole person. `size` is what the first page
  // pays, rounded, so the panel can say so before it is spent.
  //
  // ponytail: three rows. Another detector or classifier from the hub is another
  // call to personSrc or humanSrc; a classifier is a line in GENDER. Anything
  // with a library of its own needs a worker source of its own.
  var MODELS = [
    {
      id: "human",
      label: "Human",
      note: "faces, man or woman and an age, quickest and lightest",
      size: 9,
      grow: true,
      src: humanSrc(null),
    },
    {
      id: "human-fairface",
      label: "Human + FairFace",
      note: "faces, " + GENDER.fairface.note,
      size: 90,
      grow: true,
      src: humanSrc("fairface"),
    },
    {
      id: "human-rizvandwiki",
      label: "Human + rizvandwiki",
      note: "faces, " + GENDER.rizvandwiki.note,
      size: 90,
      grow: true,
      src: humanSrc("rizvandwiki"),
    },
    {
      id: "people-peta",
      label: "DETR + PETA",
      note: "whole people, " + GENDER.peta.note,
      size: 380,
      grow: false,
      src: personSrc("Xenova/detr-resnet-50", "peta"),
    },
  ];

  function modelById(id) {
    for (var i = 0; i < MODELS.length; i++) if (MODELS[i].id === id) return MODELS[i];
    return MODELS[0];
  }

  var model = modelById("__BLUR_MODEL__");

  var sheet = document.createElement("style");
  sheet.textContent =
    "." + CLASS + "{filter:blur(" + AMOUNT + "px)!important}" +
    // A region cover is a box laid over the picture, holding one patch per
    // person to hide. The patches blur what is behind them rather than the
    // element, so the rest of the picture stays as it was.
    // No z-index here: a patch layer is stacked where the picture it covers is
    // stacked, which is only known per picture, so it is set as one is placed.
    "." + CLASS + "-box{position:absolute;pointer-events:none}" +
    "." + CLASS + "-box>i{position:absolute;backdrop-filter:blur(" + AMOUNT + "px);" +
    "-webkit-backdrop-filter:blur(" + AMOUNT + "px)}" +
    // Every picture the runtime has an opinion about gets outlined with what
    // that opinion is, and says the same thing in its tooltip. Grey means seen
    // and waiting, blue means the model has it, green means nobody to blur, red
    // means blurred, orange means it could not be read at all, and purple means
    // it was too small to bother with.
    (MARKS
      ? "[data-ab-blur]{outline:3px solid #888!important;outline-offset:-3px!important}" +
        '[data-ab-blur="looking"]{outline-color:#39f!important}' +
        '[data-ab-blur="skipped"]{outline-color:#a3c!important}' +
        '[data-ab-blur="clear"]{outline-color:#2c2!important}' +
        '[data-ab-blur="blurred"]{outline-color:#e33!important}' +
        '[data-ab-blur="failed"]{outline-color:#f90!important}' +
        // One box per face found, wherever it was found, labelled with the
        // scores. Separate from the blur patches: these are drawn for every
        // face, blurred or not.
        "." + CLASS + "-marks{position:absolute!important;pointer-events:none!important;" +
        "z-index:2147483647!important}" +
        "." + CLASS + "-marks>b{position:absolute!important;box-sizing:border-box!important;" +
        "outline:2px solid #0ff!important;color:#0ff!important;background:none!important;" +
        "font:400 11px/1.2 monospace!important;white-space:nowrap!important;" +
        "text-shadow:0 0 3px #000,0 0 3px #000!important}" +
        // The panel: what this page was given, and the running verdicts. Wide
        // enough that a frame sent to the model at the default size fits inside
        // it without being scaled. Faint until it is pointed at, so it does not
        // sit over the page any more solidly than it has to.
        "#" + HUD + "{position:fixed!important;right:8px!important;bottom:8px!important;" +
        "z-index:2147483647!important;background:rgba(255,255,255,.88)!important;" +
        "color:#111!important;font:400 12px/1.5 monospace!important;padding:8px 10px!important;" +
        "border:1px solid #999!important;border-radius:6px!important;" +
        "width:520px!important;max-width:92vw!important;max-height:70vh!important;" +
        "overflow:auto!important;white-space:pre-wrap!important;margin:0!important;" +
        "box-shadow:0 2px 12px rgba(0,0,0,.35)!important}" +
        "#" + HUD + ":hover{background:#fff!important}" +
        "#" + HUD + " u{display:block!important;cursor:move!important;color:#06c!important;" +
        "text-decoration:none!important;font-weight:700!important;" +
        "user-select:none!important;-webkit-user-select:none!important}" +
        "#" + HUD + " s{display:block!important;color:#282!important;" +
        "text-decoration:none!important}" +
        "#" + HUD + " p{display:block!important;margin:6px 0 0!important;padding:4px 0 0!important;" +
        "border-top:1px solid #bbb!important}" +
        "#" + HUD + " p>span{display:block!important;color:#111!important;" +
        "margin:0 0 4px!important}" +
        // The picker. A page's own form styling would otherwise reach it.
        "#" + HUD + " select{display:block!important;width:100%!important;" +
        "margin:4px 0!important;padding:2px 4px!important;box-sizing:border-box!important;" +
        "font:inherit!important;color:#111!important;background:#fff!important;" +
        "border:1px solid #999!important;border-radius:4px!important;" +
        "text-transform:none!important;letter-spacing:normal!important}" +
        // The frame as the model was handed it: same pixels, same size, no
        // scaling by the panel or by the page.
        "#" + HUD + " canvas{display:block!important;max-width:none!important;" +
        "max-height:none!important;margin:2px 0 6px!important;border:1px solid #999!important}"
      : "");
  (document.head || document.documentElement).appendChild(sheet);

  // The corner panel. It says what settings this page was given, then one line
  // per picture saying exactly what that picture's tooltip says, so the whole
  // page can be read at once instead of hovered element by element. Built on
  // the first mark, because the body may not exist when this script runs.
  var hud = null;
  var hudCfg = null;
  var hudRows = null;
  // What the model actually came up on, filled in once the worker says. It is
  // the first thing to look at when everything is slow.
  var backend = "starting";

  function onOff(v) {
    return v ? "on" : "off";
  }

  // The panel's own placement is set the same way the rest of its styling is:
  // the page it landed in may have `!important` rules of its own, and a plain
  // inline style loses to those.
  function put(name, value) {
    hud.style.setProperty(name, value, "important");
  }

  // The settings this page was given, plus the backend, which arrives later than
  // the panel does and so is written again when it lands.
  function settings() {
    if (!hudCfg) return;
    hudCfg.textContent =
      model.note + "\n" +
      "running on " + backend + " | about " + model.size + " MB to download once\n" +
      "radius " + AMOUNT + "px | strictness " + STRICTNESS +
      " (man " + MALE_MIN.toFixed(2) + " | woman " + FEMALE_MIN.toFixed(2) +
      " | age " + MIN_AGE + "+)\n" +
      "men " + onOff(MEN) + " | women " + onOff(WOMEN) + " | sample video " + onOff(VIDEOS) + "\n" +
      "cover each person " + onOff(REGIONS) + "\n" +
      "shrink " + onOff(RESIZE) + " | image " + IMG_MAX + "px | video " + VID_MAX + "px\n" +
      "skip under " + MIN_SIZE + "px " + onOff(SKIP_SMALL);
  }

  function buildHud() {
    if (hud || !document.body) return;
    hud = document.createElement("div");
    hud.id = HUD;
    var head = document.createElement("u");
    // Plain ASCII throughout: the panel goes into whatever page it lands on, and
    // a page that is not UTF-8 would show anything else as rubbish.
    head.textContent = "blur runtime - drag to move, click to fold";
    hudCfg = document.createElement("s");
    hudRows = document.createElement("p");
    settings();

    // Dragged by its heading, which is also what folds it. The pointer is
    // captured so a drag that runs off the panel — or off the window — still
    // arrives here, and a drag that moved swallows the click that ends it.
    var moved = false;
    head.addEventListener("pointerdown", function (e) {
      var r = hud.getBoundingClientRect();
      var dx = e.clientX - r.left;
      var dy = e.clientY - r.top;
      moved = false;
      head.setPointerCapture(e.pointerId);
      head.onpointermove = function (ev) {
        moved = true;
        put("right", "auto");
        put("bottom", "auto");
        put("left", ev.clientX - dx + "px");
        put("top", ev.clientY - dy + "px");
      };
      head.onpointerup = head.onpointercancel = function () {
        head.onpointermove = head.onpointerup = head.onpointercancel = null;
      };
    });
    head.addEventListener("click", function () {
      if (moved) return;
      var folded = hudRows.style.display === "none";
      hudRows.style.display = hudCfg.style.display = pick.style.display =
        folded ? "" : "none";
    });

    // The picker. Changing it swaps the model on this page only — nothing is
    // saved, so a reload comes back to the setting. That is the point: two
    // models can be tried against the same page in the time it takes to look.
    var pick = document.createElement("select");
    for (var i = 0; i < MODELS.length; i++) {
      var opt = document.createElement("option");
      opt.value = MODELS[i].id;
      opt.textContent = MODELS[i].label + " (~" + MODELS[i].size + " MB)";
      if (MODELS[i].id === model.id) opt.selected = true;
      pick.appendChild(opt);
    }
    pick.addEventListener("change", function () {
      useModel(pick.value);
    });

    hud.appendChild(head);
    hud.appendChild(pick);
    hud.appendChild(hudCfg);
    hud.appendChild(hudRows);
    document.body.appendChild(hud);
  }

  // Change model without reloading. The old worker is thrown away mid-answer —
  // whatever it still owed is resolved as "could not tell", which blurs nothing
  // — and every picture is handed back its clean slate so the new model sees the
  // page as if it had just loaded.
  function useModel(id) {
    if (id === model.id) return;
    if (worker) worker.terminate();
    worker = null;
    broken = false;
    for (var key in waiting) waiting[key](null);
    waiting = Object.create(null);
    model = modelById(id);
    backend = "starting";
    settings();

    var all = document.querySelectorAll("img,video");
    for (var i = 0; i < all.length; i++) {
      var el = all[i];
      el.__abStop = el.__abSeen = el.__abLooked = el.__abRetried = false;
      show(el);
      outline(el, []);
    }
    sweep(document.documentElement);
  }

  // What to call a picture in the panel: its tag, the tail of its file name,
  // and the size it was handed to the model at once that is known.
  function named(el) {
    var src = el.currentSrc || el.src || el.poster || "";
    src = src.split("?")[0].split("/").pop() || src;
    if (src.length > 26) src = "..." + src.slice(-25);
    return (
      el.tagName.toLowerCase() + " " + src + (el.__abDims ? " " + el.__abDims : "")
    );
  }

  // One row per picture, made the first time it is mentioned and reused after.
  // Nothing is ever dropped: the panel is for reading what happened on a page,
  // and a row thrown away to save room is the one that would have said why.
  function row(el) {
    buildHud();
    if (!hud) return null;
    if (!el.__abRow) {
      el.__abRow = document.createElement("span");
      el.__abRow.appendChild(document.createTextNode(""));
      hudRows.appendChild(el.__abRow);
    }
    return el.__abRow;
  }

  // The last thing said about a picture is kept, so the line can be written
  // again when something other than the verdict changes.
  function say(el, text) {
    el.__abSaid = text;
    var r = row(el);
    if (r) r.firstChild.nodeValue = named(el) + " - " + text;
  }

  // The video being watched right now goes to the top of the panel. A page
  // carries a hundred thumbnails and one video, and the video's line is the one
  // worth reading while it plays — it would otherwise sit wherever in the list
  // it happened to load, scrolled out of sight.
  function pin(el) {
    if (!MARKS) return;
    var r = row(el);
    if (r && r !== hudRows.firstChild) hudRows.insertBefore(r, hudRows.firstChild);
  }

  // The frame exactly as the model got it, drawn at the size it was handed over
  // at, so what the detector had to work with can be looked at rather than
  // guessed. Drawing does not consume the bitmap, so it is still transferable
  // afterwards.
  //
  // A video would redraw on every sample, which is all the panel would ever do,
  // so it only refreshes every few.
  function thumb(el, bmp) {
    el.__abDims = bmp.width + "x" + bmp.height;
    // The size is known before the model answers, so the line is rewritten now
    // rather than left short until the verdict lands.
    if (el.__abSaid !== undefined) say(el, el.__abSaid);
    if (el.tagName === "VIDEO") {
      el.__abFrames = (el.__abFrames || 0) + 1;
      if (el.__abFrames % FRAME_EVERY !== 1) return;
    }
    var r = row(el);
    if (!r) return;
    if (!el.__abShot) {
      el.__abShot = document.createElement("canvas");
      r.appendChild(el.__abShot);
    }
    el.__abShot.width = bmp.width;
    el.__abShot.height = bmp.height;
    el.__abShot.getContext("2d").drawImage(bmp, 0, 0);
    // This frame has not been drawn on yet, so the boxes for it can go on top of
    // it and be the boxes for the frame underneath.
    el.__abFresh = true;
  }

  // The same boxes again, on the frame the model was actually handed. Read next to
  // the boxes on the page they say which half of the job is wrong: a box that sits
  // high here as well is the model reading the face high, and a box that is right
  // here and high on the page is the frame being mapped onto the element wrong.
  function traced(el, faces) {
    if (!el.__abFresh || !el.__abShot) return;
    el.__abFresh = false;
    var c = el.__abShot.getContext("2d");
    var w = el.__abShot.width;
    var h = el.__abShot.height;
    c.strokeStyle = "#0ff";
    c.lineWidth = 2;
    for (var i = 0; i < faces.length; i++) {
      var b = faces[i].box;
      c.strokeRect(b[0] * w, b[1] * h, (b[2] - b[0]) * w, (b[3] - b[1]) * h);
    }
  }

  // The tooltip carries the numbers the outline cannot. Overwriting the page's
  // own title is only acceptable because this is a debugging switch.
  function mark(el, state, note) {
    if (!MARKS) return;
    var text = state + (note === undefined ? "" : " " + note);
    el.dataset.abBlur = state;
    el.title = "blur: " + text;
    say(el, text);
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
      var url = URL.createObjectURL(new Blob([model.src], { type: "text/javascript" }));
      worker = new Worker(url);
      URL.revokeObjectURL(url);
      worker.onmessage = function (e) {
        // The worker says once whether its models came up, so a page can tell
        // "it never loaded" apart from "nobody to blur here".
        if (e.data.ready !== undefined) {
          if (e.data.ready) {
            backend = e.data.backend;
            settings();
            if (MARKS) console.log("blur: ready on " + backend);
            return;
          }
          backend = "failed";
          settings();
          if (MARKS) console.warn("blur: could not load —", e.data.why);
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

  // Hand over a frame, get back one entry per face found, or null if the model
  // could not answer.
  function findFaces(bmp) {
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
  // still has a face in it, and blurring it has to blur the whole thing.
  function tooSmall(w, h) {
    return SKIP_SMALL && (w < MIN_SIZE || h < MIN_SIZE);
  }

  // Whether this box is one of the ones to hide.
  //
  // A model that reads an age leaves children alone whichever way the switches
  // are set; one that reads no age reports null and the gate does not apply.
  //
  // A model that only finds people says "person". There is nothing to sort on,
  // so it matches whichever switches are on — asking to hide women and being
  // shown people means hiding people.
  //
  // Past that the two sides are not symmetrical: a face called female with a low
  // score is more often a man than a woman, so it counts as a man rather than as
  // nothing.
  function wanted(f) {
    if (f.age !== null && f.age < MIN_AGE) return false;
    if (f.gender === "person") return MEN || WOMEN;
    var male = (f.gender === "male" && f.score > MALE_MIN) ||
      (f.gender === "female" && f.score < MALE_MAX);
    var female = f.gender === "female" && f.score > FEMALE_MIN;
    return (male && MEN) || (female && WOMEN);
  }

  // Shrink until the longest side fits, keeping the shape, and never enlarge. A
  // picture squeezed into a square hands the detector faces no longer shaped
  // like faces, and it misses them; blowing a small picture up adds no detail
  // and only costs time. With resizing off there are no options at all and the
  // picture is read at its own size.
  function fit(w, h, max) {
    if (!RESIZE) return undefined;
    var r = Math.min(max / w, max / h, 1);
    return {
      resizeWidth: Math.max(1, Math.round(w * r)),
      resizeHeight: Math.max(1, Math.round(h * r)),
      resizeQuality: "low",
    };
  }

  // A picture is re-requested with an anonymous fetch, because one the page
  // loaded normally is tainted and cannot be read back; Adblock allows that
  // read on the response side while the blur is on, and the browser answers the
  // second request from its cache.
  function fromUrl(url, max) {
    return new Promise(function (resolve, reject) {
      var probe = new Image();
      probe.crossOrigin = "anonymous";
      probe.onload = function () {
        resolve(createImageBitmap(probe, fit(probe.naturalWidth, probe.naturalHeight, max)));
      };
      probe.onerror = reject;
      probe.src = url;
    });
  }

  // Grab and downscale in one native step. Videos are sampled straight off the
  // element — a player feeding an MSE blob is already same-origin. A video with
  // no frame decoded yet is showing its poster instead, so that is what gets
  // read: the thumbnail standing in for a video is a picture of people the same
  // as any other.
  //
  // Readiness is what says whether there is a frame, not the size: a video
  // reports its size as soon as it has metadata, a whole state before the first
  // frame is decoded.
  // ponytail: a poster is fitted into the element by its own shape, and the patch
  // layer is placed by the video's, so a poster shaped differently from the video
  // puts the patches out until the first frame arrives. Read the poster's size off
  // the bitmap if that turns up.
  function grab(el) {
    if (el.tagName === "VIDEO") {
      if (el.readyState < 2 && el.poster) return fromUrl(el.poster, IMG_MAX);
      return createImageBitmap(el, fit(el.videoWidth, el.videoHeight, VID_MAX));
    }
    return fromUrl(el.currentSrc || el.src, IMG_MAX);
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

  // What to actually cover. A person model already returns the person, so its
  // box is used as it stands. A face box says where a head is and nothing about
  // the body under it, so it is grown outwards and a long way down. All four
  // numbers are fractions of the picture.
  //
  // ponytail: fixed multiples of the face, for the face models only. This is the
  // guess the person models exist to remove.
  function area(b) {
    var w = b[2] - b[0];
    var h = b[3] - b[1];
    if (!model.grow) return { x: b[0], y: b[1], w: w, h: h };
    var x = Math.max(0, b[0] - w * 0.8);
    var y = Math.max(0, b[1] - h * 0.6);
    return {
      x: x,
      y: y,
      w: Math.min(1, b[2] + w * 0.8) - x,
      h: Math.min(1, b[3] + h * 4) - y,
    };
  }

  // Layers are positioned in page coordinates rather than wrapped around the
  // picture, so no page layout is touched. Each picture can carry two: the blur
  // patches, and the debugging outlines. A video gets both moved on every
  // sample, which is how they follow the people in it.
  //
  // ponytail: otherwise they follow scrolling and resizing. A still picture
  // moved by script or by an animation drifts away from its layers until one of
  // those happens; put a ResizeObserver on the host if that shows up.
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
    var iw = el.videoWidth || el.naturalWidth || 0;
    var ih = el.videoHeight || el.naturalHeight || 0;
    var how = css.objectFit || "fill";
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
    var pos = (css.objectPosition || "50% 50%").split(" ");
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
      var vx = seen(d.box.left, d.box.left + d.box.width, r.left, r.width);
      var vy = seen(d.box.top, d.box.top + d.box.height, r.top, r.height);
      // The patches stack where the picture stacks. Anything the page puts over
      // the picture — a play button, a caption, its own controls — stays over the
      // blur as well, instead of being blurred along with it.
      var depth = window.getComputedStyle(entry.host).zIndex;
      var keys = ["box", "marks"];
      for (var k = 0; k < 2; k++) {
        var layer = entry[keys[k]];
        if (!layer) continue;
        var s = layer.style;
        s.left = r.left + window.scrollX + "px";
        s.top = r.top + window.scrollY + "px";
        s.width = r.width + "px";
        s.height = r.height + "px";
        if (keys[k] === "box") s.zIndex = depth === "auto" ? 0 : depth;
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
      document.body.appendChild(layer);
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

  function cover(el, cells) {
    lay(el, "box", CLASS + "-box", "i", cells);
  }

  function uncover(el) {
    lay(el, "box", CLASS + "-box", "i", []);
  }

  // Every face the model found is outlined where it was found, with what the
  // model said about it — including the faces that were not blurred, which is
  // the point: a box with a low number is a face the bar turned down, and that
  // looks nothing like a face that was never seen at all. First number is how
  // sure the model is about man or woman, second is how sure the detector is
  // that this is a face, third is the age it read — a face under the age bar is
  // left alone however sure the rest of it looks.
  function outline(el, faces) {
    if (!MARKS) return;
    traced(el, faces);
    lay(
      el,
      "marks",
      CLASS + "-marks",
      "b",
      faces.map(function (f) {
        return {
          x: f.box[0],
          y: f.box[1],
          w: f.box[2] - f.box[0],
          h: f.box[3] - f.box[1],
          label: f.gender + " " + f.score.toFixed(2) + " / " + f.face.toFixed(2) +
            (f.age === null ? "" : " / " + Math.round(f.age)),
        };
      })
    );
  }

  // How many of the faces found were ones to hide, for the tooltip.
  function tally(picked, total) {
    return picked + " of " + total + (total === 1 ? " face" : " faces");
  }

  // Hide the people picked out of a frame: each one under its own patch, or the
  // whole element when region covers are switched off.
  function hide(el, picked) {
    if (!REGIONS) {
      uncover(el);
      el.classList.add(CLASS);
      return;
    }
    el.classList.remove(CLASS);
    cover(
      el,
      picked.map(function (f) {
        return area(f.box);
      })
    );
  }

  function show(el) {
    el.classList.remove(CLASS);
    uncover(el);
  }

  if (REGIONS || MARKS) {
    window.addEventListener("scroll", reflow, true);
    window.addEventListener("resize", reflow);
  }

  function checkImage(img) {
    mark(img, "looking");
    return snapshot(img)
      .then(findFaces)
      .then(function (faces) {
        if (!faces) {
          mark(img, "failed", "detector");
          return;
        }
        outline(img, faces);
        var picked = faces.filter(wanted);
        var note = tally(picked.length, faces.length);
        if (!picked.length) {
          mark(img, "clear", note);
          return;
        }
        hide(img, picked);
        mark(img, "blurred", note);
      })
      .catch(function (e) {
        mark(img, "failed", "unreadable");
        if (MARKS) console.warn("blur: cannot read", img.currentSrc || img.src, e);
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
    if (running || !queue.length) return;
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
  // the ones that are ready. Every picture is looked at whatever its size — an
  // avatar or a thumbnail holds a face the same as a photograph does.
  //
  // A loaded element still reports no size when there is nothing behind it: no
  // `src` yet, an empty one a lazy loader has not filled in, or a request that
  // failed. Waiting for `load` covers all three — it fires whenever a real
  // picture arrives, however late, and simply never fires for one that is
  // broken.
  function enqueue(img) {
    if (!img.complete || !img.naturalWidth) {
      img.addEventListener("load", function () { enqueue(img); }, { once: true });
      return;
    }
    if (tooSmall(img.naturalWidth, img.naturalHeight)) {
      mark(img, "skipped", img.naturalWidth + "x" + img.naturalHeight + ", too small");
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

  function watchImage(img) {
    // ponytail: looked at once, on the element as first seen. A carousel that
    // swaps src on a node keeps the first verdict; watch the src attribute if
    // that turns out to matter.
    if (img.__abSeen) return;
    img.__abSeen = true;
    mark(img, "queued");
    visible.observe(img);
  }

  // A video is covered the same way a picture is, only over and over: every
  // sample brings a fresh set of faces and the patches move onto them. The
  // patches lag the picture by however long a sample takes, so they are grown
  // generously rather than fitted tightly.
  function watchVideo(v) {
    if (v.__abSeen) return;
    v.__abSeen = true;
    mark(v, "queued");

    var busy = false;
    var clear = 0;
    var last = 0;

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
          v.__abStop = true;
          mark(v, "failed", "not allowed to read");
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
      if (v.__abStop || busy) return;
      // A video's own size only turns up once it has metadata, so this is
      // checked here rather than when it was first seen.
      if (v.videoWidth && tooSmall(v.videoWidth, v.videoHeight)) {
        v.__abStop = true;
        mark(v, "skipped", v.videoWidth + "x" + v.videoHeight + ", too small");
        return;
      }
      busy = true;
      // Only while it is actually playing: a paused video having its parked
      // frame looked at once is not what the reader is watching.
      if (!v.paused) pin(v);
      if (!v.__abLooked) mark(v, "looking");
      snapshot(v)
        .then(findFaces)
        .then(function (faces) {
          v.__abLooked = true;
          // No answer is nearly always a frame the page was not allowed to
          // read, which the model reports as a failure rather than throwing —
          // so the reload that asks for permission has to be tried from here
          // too, not only from the catch below.
          if (!faces) {
            if (retry()) return;
            v.__abStop = true;
            mark(v, "failed", "detector");
            return;
          }
          outline(v, faces);
          var picked = faces.filter(wanted);
          var note = tally(picked.length, faces.length);
          if (!picked.length) {
            // A face lost for one frame is usually a blink or a turn, so the
            // cover only comes off after a run of clean frames.
            if (++clear >= CLEAR_RUN) show(v);
            var still = v.classList.contains(CLASS) || (v.__abCover && v.__abCover.box);
            mark(v, still ? "blurred" : "clear", note);
            return;
          }
          clear = 0;
          hide(v, picked);
          mark(v, "blurred", note);
        })
        .then(function () {
          busy = false;
        })
        .catch(function (e) {
          busy = false;
          if (retry()) return;
          v.__abStop = true;
          mark(v, "failed", "unreadable");
          if (MARKS) console.warn("blur: cannot read video", v.currentSrc || v.src, e);
        });
    }

    function sample(now) {
      if (v.__abStop) return;
      if (!v.paused && !v.ended && !busy && v.readyState >= 2 && now - last >= SAMPLE_MS) {
        last = now;
        look();
      }
      schedule();
    }

    // requestVideoFrameCallback fires once per presented frame, so a paused or
    // stalled video costs nothing and no frame is ever sampled twice.
    function schedule() {
      if (v.__abStop || v.paused || v.ended) return;
      if (v.requestVideoFrameCallback) v.requestVideoFrameCallback(sample);
      else requestAnimationFrame(sample);
    }

    // A video nobody has played is still showing something: its poster, or the
    // frame it is parked on. That gets looked at once, whether or not video
    // sampling is on — it costs one run, not one per frame, and a thumbnail is
    // the picture a reader actually sees.
    var stilled = false;
    // A frame can only be read once one has been decoded, which is a state
    // later than having the metadata. Read a video before that and the browser
    // rejects the grab outright — which the catch above reads as a frame it is
    // not allowed to see, and stops the video for good on the strength of it.
    // Every video reaches loadedmetadata before its first frame, so that alone
    // was enough to stop them all.
    function checkStill() {
      if (stilled || v.__abStop) return;
      if (v.readyState < 2 && !v.poster) return;
      stilled = true;
      look();
    }

    v.addEventListener("loadeddata", checkStill);
    v.addEventListener("loadedmetadata", checkStill);
    checkStill();

    if (!VIDEOS) return;
    v.addEventListener("play", schedule);
    schedule();
  }

  // Videos are watched whatever the video setting says: with it off they are
  // only ever looked at once, for their poster or their parked frame.
  function sweep(root) {
    if (!root || root.nodeType !== 1) return;
    if (root.tagName === "IMG") watchImage(root);
    else if (root.tagName === "VIDEO") watchVideo(root);
    if (!root.querySelectorAll) return;
    var found = root.querySelectorAll("img,video");
    for (var i = 0; i < found.length; i++) {
      if (found[i].tagName === "IMG") watchImage(found[i]);
      else watchVideo(found[i]);
    }
  }

  new MutationObserver(function (records) {
    for (var i = 0; i < records.length; i++) {
      var added = records[i].addedNodes;
      for (var j = 0; j < added.length; j++) sweep(added[j]);
    }
  }).observe(document.documentElement, { childList: true, subtree: true });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      sweep(document.body);
    });
  } else {
    sweep(document.body);
  }
})();
