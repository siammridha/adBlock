#!/usr/bin/env bash
# Drive the already-running proxy with a real browser (agent-browser + system
# Chromium) and confirm the engine parsed and applied the proxy's HTML edits —
# the cosmetic hide rule and the blur-on-load preload CSS. This is the gap curl
# and the Rust tests can't cover: they prove the right bytes were sent, not that
# a browser acted on them.
#
# It does NOT launch a proxy. It configures whatever proxy is on
# 127.0.0.1:8080/8081, then restores every setting it changed on exit, so it
# never fights or pollutes the proxy you run during development.
set -uo pipefail
cd "$(dirname "$0")/.."

ADMIN=http://127.0.0.1:8081
PROXY=http://127.0.0.1:8080
PAGE=https://example.com/
RULE='example.com##h1'
SESSION=adblock-e2e

CHROMIUM=${CHROMIUM:-/usr/bin/chromium}
[ -x "$CHROMIUM" ] || { echo "no chromium at $CHROMIUM — apt install chromium, or set CHROMIUM"; exit 1; }
command -v agent-browser >/dev/null || { echo "agent-browser not on PATH — cargo install agent-browser"; exit 1; }

# The tests drive the running proxy; they do not start one.
saved=$(curl -s -m 5 "$ADMIN/api/adblock") || true
[ -n "$saved" ] || { echo "proxy admin API not reachable on 127.0.0.1:8081 — start the proxy first"; exit 1; }
saved_custom=$(curl -s -m 5 "$ADMIN/api/blocklist?name=custom")

browser() { agent-browser --session "$SESSION" \
  --executable-path "$CHROMIUM" --args "--no-sandbox" \
  --proxy "$PROXY" --ignore-https-errors "$@"; }

restore() {
  echo "$saved" | python3 -c 'import json,sys
s=json.load(sys.stdin)
keys=["blur","blur_men","blur_images","blur_on_load","cosmetic"]
print(json.dumps({k:s[k] for k in keys}))' \
    | curl -s -m 30 -X POST "$ADMIN/api/adblock/config" \
        -H 'content-type: application/json' -d @- >/dev/null
  echo "$saved_custom" | python3 -c 'import json,sys
t=json.load(sys.stdin).get("text","")
print(json.dumps({"name":"custom","rules":t,"replace":True}))' \
    | curl -s -m 120 -X POST "$ADMIN/api/blocklists" \
        -H 'content-type: application/json' -d @- >/dev/null
  agent-browser --session "$SESSION" close >/dev/null 2>&1
}
trap restore EXIT

# Turn the two features on and add a cosmetic rule that hides <h1> on the page.
curl -s -m 30 -X POST "$ADMIN/api/adblock/config" \
  -H 'content-type: application/json' \
  -d '{"blur":true,"blur_men":true,"blur_images":true,"blur_on_load":true,"cosmetic":true}' >/dev/null
# The POST returns only after the engine rebuilds; on the full list set that
# takes a while, so give it room.
echo "$saved_custom" | python3 -c 'import json,sys
t=json.load(sys.stdin).get("text","")
rules=(t+"\n" if t else "")+"'"$RULE"'"
print(json.dumps({"name":"custom","rules":rules,"replace":True}))' \
  | curl -s -m 120 -X POST "$ADMIN/api/blocklists" \
      -H 'content-type: application/json' -d @- >/dev/null

browser open "$PAGE" >/dev/null 2>&1

# One eval, both checks. display=='none' proves the cosmetic rule was parsed and
# applied; the abx-blur-processed marker in a parsed stylesheet proves the blur
# preload rule was too (read from cssRules, not the raw HTML).
result=$(browser eval "(()=>{const h=document.querySelector('h1');const disp=h?getComputedStyle(h).display:'no-h1';const seen=[...document.styleSheets].some(s=>{try{return[...s.cssRules].some(r=>r.cssText.includes('abx-blur-processed'))}catch(e){return false}});return disp+'|'+seen;})()" 2>/dev/null | tr -d '"')

fail=0
check() { if [ "$3" = "$2" ]; then echo "ok   $1 ($3)"; else echo "FAIL $1 (want $2, got $3)"; fail=1; fi; }
check "cosmetic rule applied in browser"  none "${result%%|*}"
check "blur preload CSS applied in browser" true "${result##*|}"

exit $fail
