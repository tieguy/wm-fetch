# AGENTS.md

Notes for agent sessions working in this repo.

## Build / test

```sh
cargo test                                  # unit + offline integration (wiremock) — no network
cargo test --release -- --ignored           # live tests against real Wikimedia (needs WM_FETCH_LIVE_CONTACT)
cargo fmt && cargo clippy --all-targets -- -D warnings   # CI enforces both
```

## SonarQube gate

Onboarded to SonarCloud (org `tieguy`, project key `tieguy_wm-fetch`). Marker
files: `.sonar-config.json` (binds the `sonar` CLI checkout) and
`sonar-project.properties` (scanner config). CI:
`.github/workflows/sonarqube.yml` generates a Clippy JSON report and runs the
SonarScanner on every push and PR; it needs the `SONAR_TOKEN` repo secret.

Before committing code changes: `sonar analyze secrets` over changed files.
After push, read `sonar list issues -p tieguy_wm-fetch --new-code --format toon`
before follow-up work counts as done. BLOCKER/HIGH findings are must-fix.

## Ground rules

- **Compliance is non-negotiable.** Do not add configuration that can
  disable the User-Agent construction, robots.txt enforcement, or pacing
  floors. Config changes identity or timing, never compliance. The
  fail-closed contact gate (no contact → exit 2) is the product — it now
  lives in the library constructor (`Session::connect_with`), so CLI and
  library consumers share one gate. `RobotsMode::RecordOnly` is the one
  deliberate library-level exception: a downstream tool owns its robots
  posture with the verdict recorded on every result; the CLI and the
  library default stay `Enforce`, and wm-fetch itself never fetches past
  a disallow.
- Policy sources (verified live 2026-09-30; CI link-checks them):
  [Robot policy](https://wikitech.wikimedia.org/wiki/Robot_policy),
  [UA policy](https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_User-Agent_Policy),
  [API:Etiquette](https://www.mediawiki.org/wiki/API:Etiquette),
  [Manual:Maxlag parameter](https://www.mediawiki.org/wiki/Manual:Maxlag_parameter),
  [ToU §12](https://foundation.wikimedia.org/wiki/Policy:Terms_of_Use).
  The README's compliance matrix is the contract — keep code and README
  in sync when behavior changes.
- The robots.txt scoped-enforcement stance (API endpoints exempt, web
  paths honored, fail-closed on fetch failure) is a documented design
  decision, not an oversight. See README "robots.txt, and the
  API-endpoint question".
- Every outbound HTTP request must go through the pacer + lock + state
  machinery (`Session::send_once`). No request kind is exempt — that
  includes robots.txt fetches and redirect hops. Since the 2.1 pacing
  change there are two buckets: **Wikimedia and other-Wikimedia-services
  hosts** share the machine-wide `state.lock` and the global `state.json`
  (bit-for-bit the old behaviour); **all other hosts** get a per-host lock
  and per-host state file (`hosts/<host>.json`) with a ≥1s floor, and
  machine-wide concurrency across different hosts capped by the slot pool
  (`global_concurrency`, default 8). `WM_FETCH_WMF_TEST_FORCE_WIKIMEDIA=1`
  is a test-only tighten override (everything classifies Wikimedia); there
  is no override in the relaxing direction.
- Exit codes: 0 ok; 1 transport/HTTP≥400/budget abort; 2 usage/config;
  3 policy refusal (three kinds — robots disallow, robots unreachable,
  other-services cooldown). `--help` must keep enumerating them
  (test `exit_codes_documented` asserts it).
- Slow-test budget: only `pacing_after_expensive` and
  `maxlag_200_retry_then_success` may sleep ≥5s. Never make the ≥5s
  floors configurable to shrink test time.
