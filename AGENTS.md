# AGENTS.md

Notes for agent sessions working in this repo.

## Build / test

```sh
cargo test                                  # unit + offline integration (wiremock) — no network
cargo test --release -- --ignored           # live tests against real Wikimedia (needs WM_FETCH_LIVE_CONTACT)
cargo fmt && cargo clippy --all-targets -- -D warnings   # CI enforces both
```

## Ground rules

- **Compliance is non-negotiable.** Do not add configuration that can
  disable the User-Agent construction, robots.txt enforcement, or pacing
  floors. Config changes identity or timing, never compliance. The
  fail-closed contact gate (no contact → exit 2) is the product.
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
- Every outbound HTTP request must go through the pacer + lock +
  state.json machinery (`Session::send_once`). No request kind is exempt
  — that includes robots.txt fetches and redirect hops.
- Exit codes: 0 ok; 1 transport/HTTP≥400/budget abort; 2 usage/config;
  3 policy refusal (three kinds — robots disallow, robots unreachable,
  other-services cooldown). `--help` must keep enumerating them
  (test `exit_codes_documented` asserts it).
- Slow-test budget: only `pacing_after_expensive` and
  `maxlag_200_retry_then_success` may sleep ≥5s. Never make the ≥5s
  floors configurable to shrink test time.
