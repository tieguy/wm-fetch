#!/usr/bin/env bash
# wm-fetch smoke test: syntax check + live, policy-shaped API round trips
# against en.wikipedia (read-only, serial, maxlag carried).
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
wf="$here/wm-fetch"

bash -n "$wf"

# Leg 1: a clean siteinfo round trip must return parseable JSON.
"$wf" "https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json&formatversion=2" |
  python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
assert d["query"]["general"]["servername"] == "en.wikipedia.org", d
print("smoke: ok (json + siteinfo)")
'

# Leg 2: the request must have carried maxlag — an INVALID value must come
# back as the API's own maxlag error, proving the parameter reaches it.
"$wf" "https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json&maxlag=abc" 2>/dev/null |
  python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
# An invalid maxlag comes back as badinteger NAMING the parameter —
# proving the request actually carried maxlag to the API.
assert d["error"]["code"] == "badinteger" and "maxlag" in d["error"]["info"], d
print("smoke: ok (maxlag reaches the API)")
'
