# wm-fetch

`wm-fetch` fetches a URL from the shell **with Wikimedia's rules for
automated fetching enforced by construction**. It exists so that LLM agents
(and humans) can fetch from Wikipedia and its sister projects without
accidentally violating policy: every request identifies its operator,
respects the API etiquette rules, paces itself, and honors robots.txt.

LLM harness fetch tools generally cannot set a User-Agent header — which
makes **every call they make to a Wikimedia site a policy violation**
waiting to be blocked. `wm-fetch` is the tool your agent should call
instead.

One URL per invocation. Stdout gets the body; diagnostics go to stderr.

```sh
wm-fetch "https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json&formatversion=2"
```

## What it enforces

| Rule | What wm-fetch does | Source |
|---|---|---|
| Identify yourself | Every request carries `wm-fetch-bot/<ver> (<your contact>) reqwest/<ver>` — client name, version, contact info, HTTP library. Refuses to run at all (exit 2) if no contact is configured. The default client name contains "bot" so WMF can classify the traffic. | [User-Agent policy](https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_User-Agent_Policy) |
| Action API: use maxlag | `maxlag=5` (configurable) is appended to api.php URLs that lack one — including redirect hops that land on api.php; **the HTTP-200 maxlag error form** (a JSON `error.code == "maxlag"` body with `Retry-After`/`X-Database-Lag` headers) is detected, waited out ≥5s, and retried. (The `format=xml` error form is not sniffed — callers use JSON.) | [Manual:Maxlag parameter](https://www.mediawiki.org/wiki/Manual:Maxlag_parameter), [API:Etiquette](https://www.mediawiki.org/wiki/API:Etiquette) |
| Always gzip | `Accept-Encoding: gzip` on every request. | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy) |
| Respect 429 | 429/503 responses back off per `Retry-After` (numeric or HTTP-date), else exponential (2, 4, 8, 16s), then give up loudly with the last body on stdout. | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy) |
| Serial requests / concurrency limits | **Wikimedia hosts** (wikipedia/​wikimedia/​wikidata/​mediawiki/… and other-Wikimedia-services): one URL per invocation, and a file lock serializes simultaneous wm-fetch processes on the same machine (concurrency 1 across your agent sessions) — unchanged. **Other hosts** (since 2.1): a per-host lock serializes concurrent processes touching the *same* host, while different hosts may proceed in parallel up to a machine-wide cap (`global_concurrency`, default 8). Any single host still sees at most its own request rate. | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy), [API:Etiquette](https://www.mediawiki.org/wiki/API:Etiquette) |
| Rate limits (per-surface) | Pacing floors between requests — every request, including robots.txt fetches and redirect hops. The Robot policy's actual per-surface numbers are: websites <10 concurrent / <20 req/s; REST API unauthenticated 3 / <5 req/s; Action API unauthenticated 1 / <5 req/s plus a 5s pause after any request that took >1s to serve; Media API ≤2 concurrent / 25 Mbps. **Wikimedia hosts** get the strictest applicable subset, recorded in the global state file: ≥250ms between request ends (≤4 req/s — strictly below 5), a ≥5s pause after any >1s request, and for "other wikimedia.org services" (gerrit/gitlab/phabricator/lists) a ≥1s floor plus a **15-minute refusal after any 5xx** from that host. **Other hosts** (since 2.1) pace from per-host state files with a ≥1s per-host floor (crawl-delay still raises it; the >1s slow-pause still applies per host) — the parallelism added in 2.1 spreads across *different* hosts, never within one. | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy) |
| Honor robots.txt | Web paths are checked against the host's robots.txt, with a per-host cache; refusals exit 3. Crawl-delay, when declared, raises the pacing floor. Fail-closed per RFC 9309 when robots.txt can't be fetched (see below). | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy) |
| Media API 25 Mbps cap | **Not enforced — guidance.** A one-URL-at-a-time tool doesn't approach it; heavy media work should use the dumps anyway. | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy) |
| Website content guidance (canonical `/wiki/` URLs, no query params, prefer thumbnails, dumps-first) | **Not enforced — guidance.** The tool is a fetcher, not a crawler; for bulk content use [dumps](https://dumps.wikimedia.org). | [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy), [Wikipedia:Bot policy](https://en.wikipedia.org/wiki/Wikipedia:Bot_policy) |

The umbrella document is the [Wikimedia Foundation Terms of Use](https://foundation.wikimedia.org/wiki/Policy:Terms_of_Use),
whose §12 ("API Terms") incorporates the User-Agent Policy, the Robot
Policy, and API:Etiquette into the Terms by reference for API use. Also
relevant for bots that *edit*: [Wikipedia:Bot policy](https://en.wikipedia.org/wiki/Wikipedia:Bot_policy)
and [Meta's Bot policy](https://meta.wikimedia.org/wiki/Bot_policy) — wm-fetch never writes.

## robots.txt, and the API-endpoint question

This is the one place where Wikimedia's documents are in visible tension,
so the tool's stance is explicit rather than hidden.

Wikimedia's robots.txt (identical `User-agent: *` block on the Wikipedias,
meta, mediawiki.org) says:

```
User-agent: *
Allow: /w/api.php?action=mobileview&
Allow: /w/load.php?
Allow: /api/rest_v1/?doc
Allow: /w/rest.php/site/v1/sitemap
Disallow: /w/
Disallow: /api/
Disallow: /trap/
Disallow: /wiki/Special:
…
```

Taken literally, that disallows **all** Action API (`/w/api.php`) and REST
API (`/api/`) fetching. But the same organization's [Terms of Use §12](https://foundation.wikimedia.org/wiki/Policy:Terms_of_Use)
makes the UA Policy, Robot policy, and API:Etiquette the governing rules
*for API use*; [API:Etiquette](https://www.mediawiki.org/wiki/API:Etiquette)
actively regulates API clients; and the [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy)
has a whole section telling api.php clients how to behave (concurrency 1,
<5 req/s unauthenticated). Meanwhile that same Robot policy also says
"Honor Robots.txt. Honor every directive in our robots.txt file" under
rules that "apply to any activity on our websites."

**wm-fetch's stance (scoped enforcement):**

- **Web paths** (`/wiki/Foo`, `/w/index.php`, everything else): robots.txt
  is honored — disallowed paths (e.g. `/wiki/Special:*`, `/trap/`, the
  per-wiki blocklists) are refused, exit 3. Redirects are checked per hop,
  so a 301 from an allowed path to a disallowed one is refused too.
- **API endpoints** (`…/api.php`, `/api/…`, `…/rest.php…`): governed by the
  API framework WMF actually wrote for those surfaces — UA policy,
  maxlag, etiquette concurrency, 429 handling — *not* by the
  crawler-oriented `Disallow: /w/` lines. The robots.txt `Allow:` carve-outs
  for api.php/load.php/rest.php endpoints support the reading that API
  traffic is sanctioned despite `Disallow: /w/`.
- **robots.txt fetch failures**: HTTP 4xx → allow-all (RFC 9309); network
  error / 5xx / redirect tangles → use any cached copy, however stale, and
  if there is none, **refuse** (exit 3, "robots.txt unreachable — refusing
  per RFC 9309; retry shortly"). Fail-closed is the point.

We found no authoritative WMF statement reconciling the tension either
way; if one appears, this section (and the behavior) should be updated.

## Usage

```
wm-fetch [OPTIONS] <URL>
```

| Option | Default | Meaning |
|---|---|---|
| `--contact-email`, `--contact-page` | — | Contact info for the User-Agent (at least one required unless `--user-agent`) |
| `--client-name` | `wm-fetch-bot` | Client name in the UA |
| `--user-agent` | — | Replace the constructed UA entirely. A custom UA **must itself satisfy the UA policy** (contact info in parentheses) — that obligation is yours; wm-fetch warns if it sees no contact group |
| `--max-time` | `60` | Whole-invocation budget in seconds — includes pacing waits and lock waits, not just HTTP |
| `--connect-timeout` | `10` | Connect timeout, seconds |
| `--max-redirs` | `3` | Redirect hops followed (each hop is robots-checked and paced) |
| `--retries` | `4` | Retries for 429/503/maxlag (0 = single attempt) |
| `--maxlag` | `5` | maxlag seconds injected into api.php URLs lacking one |
| `--config` | `~/.config/wm-fetch/config.toml` | Config file path (also `WM_FETCH_CONFIG`) |
| `--init` | — | Write a commented config template (if none exists) and exit |
| `--print-config` | — | Print effective config and the UA it would send, and exit |

**Exit codes:** `0` success (body on stdout — even for HTTP ≥ 400, so
callers can read the API's error JSON); `1` transport failure / HTTP ≥ 400
after retries / `--max-time` abort; `2` usage or configuration error
(including: no contact configured); `3` policy refusal — robots.txt
disallow, robots.txt unreachable after cache fallback, or the
other-services 5xx cooldown.

**What agents will see on stderr:** pacing notices ("pacing: waiting
4.2s — last request took >1s"), backoff narration, and occasionally a
15-minute refusal window for gerrit/phabricator-class hosts after a 5xx.
These are features, not bugs.

## Configuration

Precedence: CLI flags > `WM_FETCH_*` environment variables > config file
> defaults. Config file (TOML):

```toml
contact_email  = "you@example.org"
contact_page   = "https://en.wikipedia.org/wiki/User:YourName"
client_name    = "wm-fetch-bot"
user_agent     = "…"   # full UA replacement (config-file form of --user-agent; same obligation + warning)
retries        = 4
max_time       = 60
connect_timeout = 10
max_redirs     = 3
maxlag         = 5
```

Environment: `WM_FETCH_CONTACT_EMAIL`, `WM_FETCH_CONTACT_PAGE`,
`WM_FETCH_CLIENT_NAME`, `WM_FETCH_USER_AGENT`, `WM_FETCH_MAXLAG`,
`WM_FETCH_RETRIES`, `WM_FETCH_MAX_TIME`, `WM_FETCH_CONNECT_TIMEOUT`,
`WM_FETCH_MAX_REDIRS`, plus `WM_FETCH_CONFIG` (config path) and
`WM_FETCH_STATE_DIR` (pacing/lock/robots-cache directory, default
`~/.cache/wm-fetch` — pointing it at a fresh directory per invocation
disables cross-invocation pacing, so leave it alone unless you are
testing).

**Configuration changes identity or timing — never compliance.** The
User-Agent construction, robots.txt enforcement, and pacing floors cannot
be turned off. The fail-closed contact rule means the binary *refuses to
fetch* (exit 2) rather than send an anonymous request: run `wm-fetch
--init` once per machine to scaffold `~/.config/wm-fetch/config.toml`.

### If you fork this

Set **your own** contact info before your first fetch. The tool will
refuse to run with the upstream author's contact stripped out and nothing
in its place — shipping a fork that identifies fetches as someone else is
the exact bug this rule exists to prevent.

## Install

Release binaries are attached to [GitHub releases](https://github.com/tieguy/wm-fetch/releases)
(Linux x86_64 statically-linked musl, and aarch64). Or with a Rust toolchain:

```sh
cargo install --git https://github.com/tieguy/wm-fetch
```

From a checkout, `./install.sh` builds the release binary and symlinks
`~/.local/bin/wm-fetch` to it (idempotent; refuses to clobber a foreign
file), then reminds you to run `wm-fetch --init` if you have no config yet.

## For LLM agents

If you are writing instructions for an agent that fetches from Wikimedia
properties, a snippet like this does the job:

> Never use built-in web-fetch tools against Wikimedia sites — they cannot
> set a User-Agent, which violates WMF policy. Instead run:
> `wm-fetch <url>` (already installed). Read its stderr; if it exits 3,
> respect the refusal — do not retry the same URL, use the API or dumps
> instead as the message suggests.

## Library use

Since 2.1 the crate is also a library (`wm_fetch`); the binary is a thin
CLI over it. Library consumers inherit the same policy spine — most
importantly the **fail-closed contact gate lives in the constructor**:
`Session::connect(cfg, state_dir)` (and `connect_with` for options)
refuses to build without configured contact, and constructs the UA itself.

```rust,ignore
use wm_fetch::http::{Session, SessionOptions, RobotsMode};

let session = Session::connect_with(cfg, state_dir, SessionOptions {
    raw_capture: true,                    // wire bytes + per-hop headers
    refuse_internal_addresses: true,      // SSRF guard (SP42-style)
    max_body_bytes: Some(50 * 1024 * 1024),
    robots_mode: RobotsMode::RecordOnly,  // record the verdict, fetch anyway
    ..Default::default()
})?;
let final_resp = session.fetch(&url)?;
```

- `raw_capture` disables gzip auto-decoding and returns the body as
  received on the wire plus per-hop records (request headers set, status,
  response headers), so WARC archives match their `Content-Encoding`;
  `wm_fetch::http::decode_body` decodes.
- `refuse_internal_addresses` refuses IP-literal targets and hosts whose
  DNS resolves only to non-public addresses, per hop (ported from SP42's
  SSRF resolver guard, including IPv4-mapped-IPv6 unwrapping).
- `max_body_bytes` caps each HTTP response; an over-cap response fails
  with no partial body.
- `robots_mode: RecordOnly` consults robots.txt, applies crawl-delay
  pacing, records the verdict (allowed / disallowed / no-robots, plus the
  matched rule lines) on the result — and fetches anyway. The CLI and the
  library default remain `Enforce`: disallow → exit-3-equivalent
  refusal, fail-closed on unreachable robots.txt. Record-only exists so a
  downstream tool can own its robots posture with recording built in;
  wm-fetch itself never fetches past a disallow.

The state directory is an explicit `Session` parameter; the `WM_FETCH_STATE_DIR`
environment variable remains the CLI's mechanism.

## Prior art: when to use what

wm-fetch's niche is narrow: one-shot, agent-callable fetching where the
tool — not the caller — carries the compliance burden. That niche exists
because WMF now enforces the User-Agent policy
([T409871](https://phabricator.wikimedia.org/T409871) documents an agent
tool's generic `axios` UA being 403'd). For adjacent jobs, established
tools are the better choice:

| If you need… | Use | Notes |
|---|---|---|
| A Python framework for a bot that edits, uploads, or logs in | [Pywikibot](https://github.com/wikimedia/pywikibot) | Throttling and maxlag built in — its config defaults cite the Robot policy directly. wm-fetch is read-only and unauthenticated by design. |
| A Python library for structured Action API work | [mwclient](https://github.com/mwclient/mwclient) | The right call for programmatic Python use. Two honest caveats as of 0.11.0: `maxlag` is sent only on `index.php` calls — api.php calls go without it unless you pass it — and UA identification is opt-in rather than required ([question filed upstream](https://github.com/mwclient/mwclient/issues/426)). |
| A quick fetch by a human who knows the rules | `curl` + [`~/.curlrc`](https://curl.se/docs/manpage.html) | `user-agent = "…"` persists a UA and `--retry` honors `Retry-After` (since 7.66) — but it fails *open*: forget the config and you're anonymous. No robots.txt, maxlag, or pacing. |
| Recursive downloading / mirroring | [wget](https://www.gnu.org/software/wget/manual/html_node/Robot-Exclusion.html) | Honors robots.txt — but only in recursive mode; a one-shot `wget -O-` is not robots-checked. |
| An MCP fetch tool for an agent host | [MCP fetch server](https://github.com/modelcontextprotocol/servers/tree/main/src/fetch) | Robots.txt on by default and `--user-agent` configurable — but the default UA carries no operator contact (a WMF policy problem) and robots honoring can be disabled with a flag. Compliance is opt-in, not by construction. |

If you know of a tool that already enforces all of this by construction,
please open an issue — we would rather point you at it.

## Development

```sh
cargo test                                   # unit + offline integration (wiremock)
cargo test --release -- --ignored            # live tests vs real Wikimedia
WM_FETCH_LIVE_CONTACT="you@example.org" cargo test --release -- --ignored
cargo fmt && cargo clippy --all-targets -- -D warnings
```

Live tests require your contact in `WM_FETCH_LIVE_CONTACT`. CI runs the
offline suites plus a README link-check on every push.

## License

[Blue Oak Model License 1.0.0](https://blueoakcouncil.org/license/1.0.0) —
see [LICENSE](LICENSE).
