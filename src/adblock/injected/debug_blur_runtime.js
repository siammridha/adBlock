// The picture blur with a debugging panel wrapped around it: the outline around
// every picture the runtime has an opinion about, the box around every person
// the model found, and the corner panel listing all of it.
//
// blur_runtime.js is spliced into the bottom of this file, and this file is
// injected in its place when the blur_marks setting is on — so a page that is
// not being debugged is never sent a byte of the panel, and a page that is gets
// both in one script. Being one script is the point: the runtime runs inside
// this closure, calls report(), thumb(), outline() and ranOn() here directly,
// and hands its handle to CONTROL() below.
//
// The runtime starts nothing by itself here. It waits for the panel's own "run
// the blur" switch, which starts unticked, so a page loads with the panel up and
// nothing looked at until it is asked for.
//
// The two placeholders below are filled in before this is injected: the version
// one with the build tag, and the runtime one at the bottom with the whole of
// blur_runtime.js. Neither is spelled out here, because a placeholder written in
// a comment is replaced along with the real one.
(function () {
  var HUD = "abx-blur-hud"; // the corner panel
  var VERSION = "__BLUR_VERSION__";

  // The runtime's handle, once it has spliced itself in below and called
  // CONTROL. Nothing in the panel runs before that.
  var blur = null;
  var CLASS = "";

  // The panel's own stylesheet. Separate from the runtime's: that one carries
  // the hold and the blur, this one carries the debugging marks, and neither
  // needs to know the other exists.
  //
  // Every picture the runtime has an opinion about gets outlined with what that
  // opinion is, and says the same thing in its tooltip. Grey means seen and
  // waiting, blue means the model has it, green means nobody to blur, red means
  // blurred, orange means it could not be read at all, and purple means it was
  // too small to bother with.
  function styles() {
    var sheet = document.createElement("style");
    sheet.textContent =
      "[data-ab-blur]{outline:3px solid #888!important;outline-offset:-3px!important}" +
      '[data-ab-blur="looking"]{outline-color:#39f!important}' +
      '[data-ab-blur="skipped"]{outline-color:#a3c!important}' +
      '[data-ab-blur="clear"]{outline-color:#2c2!important}' +
      '[data-ab-blur="blurred"]{outline-color:#e33!important}' +
      '[data-ab-blur="failed"]{outline-color:#f90!important}' +
      // One box per person found, wherever it was found, labelled with
      // the class and the score. Separate from the blur patches: these are
      // drawn for every person, blurred or not.
      "." + CLASS + "-marks{position:absolute!important;pointer-events:none!important;" +
      "z-index:2147483647!important}" +
      "." + CLASS + "-marks>b{position:absolute!important;box-sizing:border-box!important;" +
      "outline:2px solid #0ff!important;color:#0ff!important;background:none!important;" +
      "font:400 11px/1.2 monospace!important;white-space:nowrap!important;" +
      "text-shadow:0 0 3px #000,0 0 3px #000!important}" +
      // The panel's switch for those boxes. On the root element, so layers
      // added after it was turned off are hidden too.
      "html." + CLASS + "-nobox ." + CLASS + "-marks{display:none!important}" +
      // The panel: what this page was given, and the running verdicts. Faint
      // until it is pointed at, so it does not sit over the page any more
      // solidly than it has to.
      // A column that does not scroll itself, so only the list of rows does
      // and the heading stays where it can be grabbed and clicked.
      "#" + HUD + "{position:fixed!important;right:8px!important;bottom:8px!important;" +
      "z-index:2147483647!important;background:rgba(255,255,255,.88)!important;" +
      "color:#111!important;font:400 12px/1.5 monospace!important;padding:8px 10px!important;" +
      "border:1px solid #999!important;border-radius:6px!important;" +
      "width:520px!important;max-width:92vw!important;max-height:70vh!important;" +
      "display:flex!important;flex-direction:column!important;" +
      "overflow:hidden!important;white-space:pre-wrap!important;margin:0!important;" +
      "box-shadow:0 2px 12px rgba(0,0,0,.35)!important}" +
      "#" + HUD + ":hover{background:#fff!important}" +
      "#" + HUD + " u{display:block!important;flex:0 0 auto!important;" +
      "cursor:move!important;color:#06c!important;" +
      "text-decoration:none!important;font-weight:700!important;" +
      "user-select:none!important;-webkit-user-select:none!important}" +
      "#" + HUD + " s{display:block!important;flex:0 0 auto!important;color:#282!important;" +
      "text-decoration:none!important}" +
      "#" + HUD + " label{display:block!important;flex:0 0 auto!important;" +
      "cursor:pointer!important;color:#111!important;" +
      "user-select:none!important;-webkit-user-select:none!important}" +
      "#" + HUD + " label input{vertical-align:middle!important;" +
      "margin:0 5px 2px 0!important}" +
      // min-height:0 or the list refuses to shrink below its content and
      // pushes the heading off the top of the panel instead of scrolling.
      "#" + HUD + " p{display:block!important;flex:1 1 auto!important;min-height:0!important;" +
      "overflow-y:auto!important;margin:6px 0 0!important;padding:4px 0 0!important;" +
      "border-top:1px solid #bbb!important}" +
      "#" + HUD + " p>span{display:block!important;color:#111!important;" +
      "margin:0 0 4px!important}" +
      // The frame as the model was handed it, scaled down to the width of the
      // panel. Drawn at its own size it is wider than the panel and the right
      // of every frame is cut off, which is the half a box is most often in.
      "#" + HUD + " canvas{display:block!important;width:100%!important;" +
      "height:auto!important;margin:2px 0 6px!important;border:1px solid #999!important}";
    (document.head || document.documentElement).appendChild(sheet);
  }

  // The corner panel. It says what settings this page was given, then one line
  // per picture saying exactly what that picture's tooltip says, so the whole
  // page can be read at once instead of hovered element by element. Built as
  // soon as the body exists, because it is what starts the blur and nothing
  // else on this page is going to happen until it is up.
  var hud = null;
  var hudCfg = null;
  var hudRows = null;
  // What the model actually came up on, filled in once the worker says. It is
  // the first thing to look at when everything is slow. The worker is not built
  // until a frame is sent to it, so there is nothing to say until the blur has
  // been run.
  var backend = "not started";
  // What the panel's own switch has been set to.
  var runState = "not running";

  function onOff(v) {
    return v ? "on" : "off";
  }

  // What the worker came up on, once it says. The panel is written again with
  // it, because it was drawn before the answer arrived.
  function ranOn(name) {
    backend = name;
    settings();
  }

  // The panel's own placement is set the same way the rest of its styling is:
  // the page it landed in may have `!important` rules of its own, and a plain
  // inline style loses to those.
  function put(name, value) {
    hud.style.setProperty(name, value, "important");
  }

  // The settings this page was given, what the switch is set to, and the
  // backend, which arrives later than the panel does and so is written again
  // when it lands.
  function settings() {
    if (!hudCfg) return;
    var c = blur.config;
    hudCfg.textContent =
      "HaramBlur, whole people, woman/man/girl/person\n" +
      "blur " + runState + " | model on " + backend +
      " | about 12 MB to download once\n" +
      "radius " + c.amount + "px | strictness " + c.strictness +
      " (a person at " + c.score.toFixed(2) + ")\n" +
      "men " + onOff(c.men) + " | women+girls " + onOff(c.women) +
      " | images " + onOff(c.images) + " | videos " + onOff(c.videos) + "\n" +
      "cover each person " + onOff(c.regions) + " | grayscale " + onOff(c.gray) +
      " | until checked: " + (c.onLoad ? "blurred" : "hidden") + "\n" +
      "unblur on hover: images " + onOff(c.hoverImages) +
      " | videos " + onOff(c.hoverVideos) + "\n" +
      "frame " + c.modelSize + "px square | shrink to " + c.imageCap + "px images, " +
      c.videoCap + "px video | skip under " + c.minSize + "px\n" +
      "build " + VERSION;
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

    // The switch this whole file exists for. Until it is ticked the runtime has
    // done nothing at all; this is what lets it go, and unticking it stops it
    // again. Unticking is a stop, not an undo: pictures already blurred stay
    // blurred, because revealing one nobody has re-checked would show exactly
    // what the blur was there for.
    var runner = document.createElement("label");
    var runOn = document.createElement("input");
    runOn.type = "checkbox";
    runOn.checked = false;
    runOn.addEventListener("change", function () {
      if (runOn.checked) {
        runState = "running";
        if (backend === "not started") backend = "starting";
        blur.run();
      } else {
        runState = "stopped";
        blur.stop();
      }
      settings();
    });
    runner.appendChild(runOn);
    runner.appendChild(document.createTextNode("run the blur"));

    // Outside the heading, so ticking it neither folds the panel nor drags it,
    // and so it stays reachable while the panel is folded.
    var toggle = document.createElement("label");
    var boxOn = document.createElement("input");
    boxOn.type = "checkbox";
    boxOn.checked = true;
    boxOn.addEventListener("change", function () {
      document.documentElement.classList.toggle(CLASS + "-nobox", !boxOn.checked);
    });
    toggle.appendChild(boxOn);
    toggle.appendChild(document.createTextNode("boxes around each person"));

    // The video sample rate, in frames a second. The runtime reads its rate
    // afresh on every frame, so moving this changes the rate on the very next
    // one. It is a live tweak only, like the boxes switch: nothing persists, a
    // reload goes back to the built-in rate.
    var rate = document.createElement("label");
    var rateText = document.createTextNode("");
    var rateIn = document.createElement("input");
    rateIn.type = "range";
    rateIn.min = "1";
    rateIn.max = "30";
    rateIn.step = "1";
    rateIn.value = String(Math.round(1000 / blur.rate()));
    function showRate() {
      rateText.nodeValue = "sample videos at " + rateIn.value + " fps ";
    }
    showRate();
    rateIn.addEventListener("input", function () {
      blur.rate(1000 / Number(rateIn.value));
      showRate();
    });
    rate.appendChild(rateText);
    rate.appendChild(rateIn);

    // Dragged by its heading, which is also what folds it. The pointer is
    // captured so a drag that runs off the panel — or off the window — still
    // arrives here, and a drag that moved swallows the click that ends it.
    //
    // A click has to be allowed to drift: no hand holds a mouse perfectly still
    // between press and release, and counting any movement at all as a drag ate
    // every fold. Nothing under this many pixels moves the panel.
    var SLOP = 4;
    var moved = false;
    head.addEventListener("pointerdown", function (e) {
      var r = hud.getBoundingClientRect();
      var dx = e.clientX - r.left;
      var dy = e.clientY - r.top;
      var fromX = e.clientX;
      var fromY = e.clientY;
      moved = false;
      head.setPointerCapture(e.pointerId);
      head.onpointermove = function (ev) {
        if (
          !moved &&
          Math.abs(ev.clientX - fromX) + Math.abs(ev.clientY - fromY) < SLOP
        ) {
          return;
        }
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
    // Both halves are hidden with `important`, because the rules that lay them
    // out are `important` too and a plain inline `display` loses to those. That
    // is why folding did nothing at all.
    head.addEventListener("click", function () {
      if (moved) return;
      var show = hudRows.style.display === "none" ? "block" : "none";
      hudRows.style.setProperty("display", show, "important");
      hudCfg.style.setProperty("display", show, "important");
    });

    hud.appendChild(head);
    hud.appendChild(runner);
    hud.appendChild(toggle);
    hud.appendChild(rate);
    hud.appendChild(hudCfg);
    hud.appendChild(hudRows);
    document.body.appendChild(hud);
  }

  // What to call a picture in the panel: its tag, the tail of its file name,
  // and the size it was handed to the model at once that is known.
  function named(el) {
    var src = el.currentSrc || el.src || el.poster || el.__abBg || "";
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

  // Newest verdict first. A page carries a hundred pictures and the list is
  // longer than the panel, so the row worth reading is the one that just
  // changed. A playing video answers several times a second and stays at the
  // top for as long as it plays, without anything having to know it is a video.
  function top(el) {
    var r = row(el);
    if (r && r !== hudRows.firstChild) hudRows.insertBefore(r, hudRows.firstChild);
  }

  // The frame exactly as the model got it, drawn at the size it was handed over
  // at, so what the detector had to work with can be looked at rather than
  // guessed. Drawing does not consume the bitmap, so it is still transferable
  // afterwards. A video redraws on every sample, so the panel shows every frame
  // sent for detection.
  function thumb(el, bmp) {
    el.__abDims = bmp.width + "x" + bmp.height;
    // The size is known before the model answers, so the line is rewritten now
    // rather than left short until the verdict lands.
    if (el.__abSaid !== undefined) say(el, el.__abSaid);
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
  // high here as well is the model reading the person high, and a box that is
  // right here and high on the page is the frame being mapped onto the element
  // wrong.
  function traced(el, people) {
    if (!el.__abFresh || !el.__abShot) return;
    el.__abFresh = false;
    var c = el.__abShot.getContext("2d");
    var w = el.__abShot.width;
    var h = el.__abShot.height;
    c.strokeStyle = "#0ff";
    c.lineWidth = 2;
    for (var i = 0; i < people.length; i++) {
      var b = people[i].box;
      c.strokeRect(b[0] * w, b[1] * h, (b[2] - b[0]) * w, (b[3] - b[1]) * h);
    }
  }

  // Every person the model found is outlined where it was found, labelled with
  // the class it was read as and how sure the model is — including the ones that
  // were not blurred, which is the point: a box marked person is somebody the
  // switches deliberately left alone, and that looks nothing like somebody who
  // was never seen at all. The number appears twice because this model has one
  // score for both, unlike the pipeline on the other branch.
  function outline(el, people) {
    traced(el, people);
    blur.lay(
      el,
      "marks",
      CLASS + "-marks",
      "b",
      people.map(function (f) {
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

  // What mark() calls with every verdict. The tooltip carries the numbers the
  // outline cannot. Overwriting the page's own title is only acceptable because
  // this is a debugging switch.
  function report(el, state, note) {
    var text = state + (note === undefined ? "" : " " + note);
    el.dataset.abBlur = state;
    el.title = "blur: " + text;
    say(el, text);
    if (state !== "queued" && state !== "looking") top(el);
  }

  // The runtime's way in, hoisted so it is already here when the runtime spliced
  // in below runs. It hands over its handle instead of starting, and everything
  // above is what decides when it does start.
  //
  // The panel is built now rather than on the first verdict, because there will
  // be no verdicts until somebody ticks a box on it.
  function CONTROL(api) {
    blur = api;
    CLASS = api.CLASS;
    styles();
    if (document.body) buildHud();
    else document.addEventListener("DOMContentLoaded", buildHud, { once: true });
  }

  __BLUR_RUNTIME__
})();
