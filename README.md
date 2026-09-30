# wm-fetch

`wm-fetch` — fetch a URL from the shell with a Wikimedia-compliant
User-Agent. This is the tool `~/Projects/CLAUDE.md` instructs every
project session to use *instead of* harness fetch tools (which cannot set
a User-Agent — a policy violation on every call).

**Canonical home:** this repo (`~/Projects/wiki/wm-fetch`). The installed
`~/.local/bin/wm-fetch` is a symlink here — never let a divergent copy
live elsewhere again; that is how v1.0 got lost.

## What it does

- **Compliant UA** (`wm-fetch/1.1 (User page + email) curl/x.y`) per the
  [WMF User-Agent policy]; the contact identifies the operator, and any
  fork edits `CONTACT_EMAIL`/`CONTACT_PAGE` — the fork-edit rule from
  wikiactive applies here too.
- **maxlag=5** appended to Action API URLs (`*api.php*`) when absent —
  [API:Etiquette].
- **gzip** (`--compressed`), **bounded redirects** (`-L --max-redirs 3`),
  **bounded time** (`--connect-timeout 10 --max-time 60`) — a wedged
  socket fails instead of hanging a session.
- **429/503 backoff**: honors a numeric `Retry-After`, else exponential
  (2, 4, 8, 16s), up to 4 attempts, then a loud exit 1.
- Serial by construction: one URL per invocation; no parallel bursts.
- Errors: body still printed on HTTP ≥ 4 (so callers can see the API's
  error JSON), exit code 1 on transport failure or HTTP ≥ 400, 2 on usage.

## Usage

```
wm-fetch <url> [extra curl args...]
```

Extra curl args come AFTER the built-ins, so callers can override them —
notably `-A` for a project's own registered UA (wikiactive and friends
hardcode theirs; they should pass theirs). Do not pass `-o`, `-D`, or
`-w` (used internally).

## Install

```
./install.sh          # symlinks ~/.local/bin/wm-fetch -> this repo's copy
```

Symlink, not copy: the repo stays the single source of truth and
upgrades take effect immediately on every machine that symlinks here.

## Tests

```
tests/smoke.sh        # bash -n + live en.wikipedia round trips
```

The live legs assert: parseable siteinfo JSON on a clean call; and that
the request actually carried `maxlag` (an invalid `maxlag=abc` must come
back as the API's own `maxlag` error — proving the parameter reaches the
API).

## Version history

- **1.1** (2026-09-29, this repo's first hardened pass): temp-file cleanup
  via `trap` (interrupts no longer leak `mktemp` files), bounded requests
  (`--connect-timeout`/`--max-time`), bounded redirect following
  (`-L --max-redirs 3`), UA gains the operator's on-wiki contact page
  alongside the email, quoted temp paths, documented caller overrides.
- **1.0** (imported verbatim from the operator's other machine): UA
  policy compliance, maxlag injection, gzip, Retry-After-aware backoff.
  Never committed anywhere before this repo — that was the bug.

[WMF User-Agent policy]: https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_User-Agent_Policy
[API:Etiquette]: https://www.mediawiki.org/wiki/API:Etiquette
