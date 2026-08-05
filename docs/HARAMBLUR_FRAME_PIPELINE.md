# How HaramBlur moves video frames, and how we would do it

> **This describes an old version.** The GitHub repository stops at 0.2.6 (July
> 2024). The extension people actually install is 0.4.1 (August 2025), which was
> never pushed there and is licensed "All Rights Reserved" rather than AGPL. 0.4.1
> replaced the Human face pipeline described below with its own YOLOv8 model —
> classes `Woman`, `Man`, `Girl`, `Person`, one score bar at 0.33–0.35,
> `maxDetected: 70` — and added per-person blurring (`specificBlur`, on by
> default). What is still true below is the frame-moving argument in sections 3 to
> 5; the model details are 0.2.6's. Section 7 covers 0.4.1's settings, which this
> project's picture blur now follows.

Source read: [alganzory/HaramBlur](https://github.com/alganzory/HaramBlur), `main`
branch, version 0.2.6 (last code commit July 2024). Files that matter:
`src/modules/processing2.js`, `src/offscreen.js`, `src/background.js`,
`src/modules/helpers.js`, `src/modules/detector.js`, `manifest.json`.

---

## 1. The short answer

HaramBlur does **not** use one video API. It uses a chain of five:

| Step | API |
| ---- | --- |
| Pick when to grab a frame | `requestAnimationFrame`, throttled to 25 fps by a `performance.now()` check |
| Copy the frame out of the video | `CanvasRenderingContext2D.drawImage(video, …)` onto an `OffscreenCanvas` |
| Turn it into bytes | `OffscreenCanvas.convertToBlob({ type: "image/jpeg", quality: 0.6 })` |
| Hand it to the backend | `URL.createObjectURL(blob)` → `chrome.runtime.sendMessage()` |
| Read it back on the other side | `new Image()`, `img.src = blobUrl`, then `tf.browser.fromPixels(img)` |

There is no "backend" in the server sense. The "backend" is an **offscreen
document** — a hidden extension page (`chrome.offscreen.createDocument`,
`manifest.json` permission `offscreen`). It exists because an MV3 service worker
has no DOM and no WebGL, so the model cannot run there. Detection uses
[Human](https://github.com/vladmandic/human) on the `humangl` (WebGL) backend for
faces, plus NSFWJS for content class.

## 2. The exact flow

**Content script** (`processing2.js`):

1. `videoDetectionLoop()` runs on `requestAnimationFrame`. It skips work unless
   at least 40 ms passed (`FRAME_RATE = 1000 / 25`), and unless the previous
   frame already came back (`activeFrame` flag). One frame in flight at a time.
2. `processFrame()` draws the video onto a reused `OffscreenCanvas`, sized down
   by `calcResize()` to at most 1920/4.5 × 1080/4.5 ≈ 427×240.
3. The canvas is JPEG-encoded at quality 0.6 into a `Blob`.
4. `URL.createObjectURL(blob)` gives a `blob:` URL string. That string, plus
   `video.currentTime`, is sent with `chrome.runtime.sendMessage`.
5. The reply comes back in the callback. The blob URL is revoked. If the reply
   is `"skipped"`, or if the frame is more than 0.5 s behind the video's current
   time, it is dropped.

**Offscreen document** (`offscreen.js`):

6. `handleVideoDetection()` receives the message. If a frame is already being
   processed it answers `{ result: "skipped" }` immediately.
7. It sets `frameImage.src = data` on a single reused `Image`, waits for
   `onload`, then runs `tf.browser.fromPixels(img)` to get a tensor.
8. NSFW classify first. If that passes and gender detection is on, Human's face
   model runs. The answer is `"nsfw"`, `"face"`, or `false`.

**Back in the content script**: `processVideoDetections()` smooths the result.
One positive frame blurs; three consecutive negative frames unblur. The blur
itself is a CSS class, `hb-blur`.

Still images take a different path: only the `src` URL is sent, and the offscreen
document re-downloads the image itself with `new Image()` and
`crossorigin=anonymous`. No pixels cross the message boundary for images.

## 3. Why the pipeline is shaped this way

`chrome.runtime.sendMessage` serializes messages to JSON. It cannot carry a
`Blob`, an `ImageBitmap`, an `ImageData`, or a `VideoFrame`, and it has no
transfer list. So pixels cannot be passed directly. HaramBlur's workaround is to
encode the frame as JPEG and pass a URL string that points at it.

That workaround costs, per frame, at 25 fps:

- a full JPEG encode,
- a full JPEG decode,
- a lossy image (quality 0.6) fed to a model that was trained on clean pixels,
- one round trip through the extension message bus,
- an `Image` load event, which is a task on the offscreen document's event loop.

The `drawImage` → canvas step also forces a GPU→CPU readback of the video frame,
which stalls the pipeline. `willReadFrequently: true` on the context makes the
canvas CPU-backed to soften that, at the price of a slower `drawImage`.

Also worth noting: the single shared `frameImage` and the single `activeFrame`
flag mean the whole extension detects one frame at a time across every tab.

## 4. What is already dated in it

The code targets Chrome 109 and was written before the following were widely
available. All are baseline in Chrome today:

- `HTMLVideoElement.requestVideoFrameCallback()` — fires once per *presented*
  frame and hands you `mediaTime` and `presentedFrames`. Replaces the
  rAF + `performance.now()` throttle and stops you sampling the same frame twice.
- `createImageBitmap(video, { resizeWidth, resizeHeight, resizeQuality })` —
  grabs and downscales in one native step. No canvas, no `drawImage`, no encode.
- `ImageBitmap` and `VideoFrame` are **transferable**. `postMessage` to a Worker
  moves them with zero copy.
- WebGPU backend for TF.js, and WebCodecs `VideoFrame` for direct texture upload.
- `OffscreenCanvas.transferToImageBitmap()` if a canvas is still in the path.

## 5. How we would do it here, better and faster

Our project is a MITM proxy, not an extension. That removes the entire reason
HaramBlur's pipeline is expensive: **there is no process boundary to cross.**

`adblock/` already injects a runtime into every HTML response
(`src/adblock/rewrite.rs:70`, `cosmetic_runtime.js`, `procedural_runtime.js`).
A blur runtime is another injected script. It runs **in the page**, where the
`<video>` element lives. The model and the video are in the same JS context.
Nothing is serialized, nothing is encoded, nothing is messaged.

The whole loop becomes:

```js
video.requestVideoFrameCallback(async function tick(now, meta) {
  const bmp = await createImageBitmap(video, {
    resizeWidth: 224, resizeHeight: 224, resizeQuality: "low",
  });
  const verdict = await detect(bmp);   // tf.browser.fromPixels(bmp)
  bmp.close();
  video.classList.toggle("hb-blur", verdict);
  video.requestVideoFrameCallback(tick);
});
```

Compared to HaramBlur, per frame that removes: one canvas draw, one GPU readback,
one JPEG encode, one JPEG decode, one blob URL create/revoke pair, one extension
message round trip, and one image load event. It also removes the lossy JPEG, so
the model sees the real pixels.

If the model turns out to be too slow on the main thread and needs a Worker,
transfer the bitmap instead of encoding it:

```js
worker.postMessage({ bmp, mediaTime: meta.mediaTime }, [bmp]);  // zero copy
```

That is the one line HaramBlur could not write, because `chrome.runtime.sendMessage`
has no transfer list and `Worker.postMessage` does.

### Where the pieces go under our architecture rules

- **`adblock/`** owns all of it: the injected blur runtime, the on/off switch, the
  blur strength setting, fetching the model files, and serving them. Serving is
  the same mechanism as `$redirect` — a request for the model URL comes through,
  `adblock` answers it with a stand-in body. See `check_request` / `blocked_response`
  in `src/adblock/api.rs`.
- **`proxy/`** does nothing new. It already hands every response to `adblock` and
  forwards what comes back. It must not know this feature exists.
- **`webapp/`** gets a toggle and a slider that call the `adblock` API. No logic.
- **`stats/`** can receive counts if we want them, through the existing call.

### What we would keep from HaramBlur

Two things, both cheap and both correct:

1. **Hysteresis.** Blur on one positive frame, unblur only after several negative
   frames. Stops flicker.
2. **Stale-frame drop.** If the verdict arrives more than ~0.5 s behind the
   video's current time, throw it away rather than act on it.

### What we would not do server-side

Running detection in Rust on frames passing through the proxy sounds tidy, but it
means demuxing and decoding HLS/DASH segments in the proxy, holding video bodies
in memory instead of streaming them, and re-encoding to blur. That is a video
transcoder, not a proxy, and it would break core principle 6 in spirit even if
`adblock` did the work. Detection belongs in the browser, where the decoded frame
already exists for free. The proxy's job is to get the runtime into the page.

## 6. One caveat about the model

Face and NSFW models are 20–30 MB of weights. HaramBlur ships them inside the
extension package. We would have to serve them through the proxy on first use and
let the browser cache them, which means the first page with a video pays a real
download. Worth measuring before committing to this feature.

## 7. 0.4.1's settings, and which of them we have

Read off `dist/popup/popup.html` and the defaults object in `dist/content.js`.
Every one of these is now a setting in `adblock/`, under the same meaning and the
same default, so the dashboard's picture blur behaves the way the extension does
out of the box.

| HaramBlur | Default | Ours |
| --------- | ------- | ---- |
| `status` | on | `blur` — off, because ours downloads a model on first use |
| `blurAmount`, 10–50 | 25 | `blur_amount` |
| `gray` | on | `blur_gray` |
| `specificBlur` | on | `blur_regions` |
| `strictness`, 0.1–1 | 0.4 | `blur_strictness`, the same range as a percentage |
| `blurMale` | off | `blur_men` |
| `blurFemale` | on | `blur_women` |
| `blurImages` | on | `blur_images` |
| `blurVideos` | on | `blur_videos` |
| `blurryStartMode` | off | `blur_on_load` |
| `unblurImages` | off | `blur_hover_images` |
| `unblurVideos` | off | `blur_hover_videos` |
| `detectionModel` | one of three | `blur_model`, our own list |
| `whitelist` | empty | the proxy's excluded hosts, which already exist |
| `hideVideoToggle` | off | **not ours** — it hides a per-video button we do not draw |

`passwordProtectionEnabled`, `companionMode` and `trialWelcomeShown` are account
features, not blur settings, and have no place here.

What the switches do to a picture is taken from the extension's stylesheet as
well: `filter: blur(Npx) grayscale(100%)` with a 0.1s transition on the element,
and for per-person blurring a patch with `backdrop-filter`, a 5px radius and a
faint white wash. `blurryStartMode` is an animation rather than a class that
waits, so that a detector which never answers cannot leave a page unreadable —
HaramBlur caps it at 20 seconds and so do we.

Four settings are ours and have no HaramBlur equivalent: the frame sizes a
picture is shrunk to (`blur_resize`, `blur_img_size`, `blur_video_size`), the
size below which a picture is not worth looking at (`blur_skip_small`,
`blur_min_size`), the choice of detector, and `blur_marks` — the overlay that
outlines every picture with the verdict it got and lists them in a panel.
HaramBlur feeds its detector one fixed size and has nothing to debug with.
