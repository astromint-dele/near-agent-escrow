# ClaimAttestor: evidence-checked web claims on GenLayer

A small reusable primitive: turn "is this claim true according to this page?" into
an on-chain record **with a verbatim citation** that validators actually checked.

```
submit_claim("The bridge reopened in March 2026.", "https://example.org/report")  -> 0
resolve(0)  -> "supported"
get_claim(0) -> {verdict: "supported",
                 quote: "The bridge reopened to traffic on 3 March 2026.", ...}
```

Other contracts can call `get_claim(id)` and act only on `status == "resolved"`
with a verdict they trust: payouts, insurance triggers, bounty checks, moderation.

## How consensus is used

`resolve()` runs one non-deterministic block via `gl.vm.run_nondet_unsafe`.

**Leader.** It fetches the URL, strips the HTML to text, and asks the LLM for
`{verdict, quote, reason}`. If the model's quote is not actually on the page,
the leader itself downgrades the result to `not_found`. Hallucinated citations
never leave the leader.

**Validators** audit the leader's evidence rather than redo the whole task:

| Leader verdict | Validator check | Kind |
|---|---|---|
| `supported` / `refuted` | The quote must appear in the validator's **own** fetch of the page | deterministic |
| `supported` / `refuted` | Its own LLM, shown only the claim and the quote, must reach the same verdict | LLM |
| `not_found` | Re-runs the search and **disagrees if it finds valid evidence** the leader missed | LLM + deterministic |
| any | Shape and length checks on the result | deterministic |

Why this shape:
- Comparing "verdict given this quote" is far more stable across models than
  comparing two free-form searches, so honest validators converge.
- The substring check means a lying leader cannot invent evidence.
- The `not_found` rule stops a lazy or malicious leader from burying a claim by
  always answering "not found".

Fetch errors (HTTP 4xx and 5xx) resolve to `not_found` with the status code in
`reason`, so a dead link reaches consensus instead of stalling.

Prompt-injection hardening: the page and the quote sit in tagged blocks and the
prompts mark them as untrusted data. The tests include a page that says "Ignore
previous instructions…". The deterministic quote check is the real backstop.

## State

```
claims:  TreeMap[u256, Claim]   # Claim is an @allow_storage dataclass
next_id: u256
Claim = submitter, text, url, status ("open" | "resolved"),
        verdict, quote, reason, resolver
```

- A claim is resolved **once** and is final afterwards.
- Input limits: claim up to 280 characters, a single `https://` URL up to
  300 characters. Page text is capped at 12,000 characters and quotes at
  20–400 characters.
- Storage is written only after consensus. The nondet block works on local
  copies of the claim and URL.

## Methods

| Method | Kind | Notes |
|---|---|---|
| `submit_claim(text, url) -> int` | write | Returns the claim id |
| `resolve(claim_id) -> str` | write | Runs consensus. Anyone may call it, once per claim |
| `get_claim(claim_id) -> dict` | view | |
| `get_claim_count() -> int` | view | |

## Tests (direct mode, no Docker)

```bash
python -m venv .venv
.venv/Scripts/python -m pip install -r requirements.txt   # Windows
.venv/Scripts/python -m pytest tests/ -v
```

There are 11 tests:
- the supported path and resolve-once
- the leader dropping a hallucinated quote
- an HTTP 404 resolving to `not_found`
- validator disagreement: quote missing from its own fetch, a different
  verdict, evidence the leader missed
- validator agreement on `not_found`
- input validation

Two environment workarounds, both confined to `tests/`:
- `GENVM_VERSION` defaults to `v0.2.16`. genlayer-test 0.29.2 picks the newest
  GenVM release, but the `v0.3.0-rc*` releases ship no `genvm-universal.tar.xz`.
- `tests/conftest.py` patches a Windows-only temp-file bug in genlayer-test's
  direct loader. It does nothing on Linux or macOS.

## Deployment

| Network | Address |
|---|---|
| GenLayer Studio | [`0x3AfE1AC9A56CDA62B8EB27E31445557F6f6c2a52`](https://explorer-studio.genlayer.com/address/0x3AfE1AC9A56CDA62B8EB27E31445557F6f6c2a52) |

Open it in Studio: https://studio.genlayer.com/?import-contract=0x3AfE1AC9A56CDA62B8EB27E31445557F6f6c2a52

### Live run (Studio, full consensus)

```
submit_claim("Python was first released in 1991",
             "https://en.wikipedia.org/wiki/Python_(programming_language)")
resolve(0)    -> FINALIZED
get_claim(0)  -> verdict: "supported"
                 quote:   "Van Rossum first released it in 1991 as Python 0.9.0."
                 reason:  "The page explicitly states Python was first released in 1991."
```

An earlier deployment exposed a real bug. It quoted template markup that leaked
out of a Wikipedia `data-mw` attribute: the quote was on the raw page, so the
substring check passed, but it wasn't human-readable. The tag stripper now skips
over quoted attribute values, and
`test_markup_inside_attributes_is_not_page_text` reproduces the case.

## Deploy

Deploy to the GenLayer **Bradbury** testnet (chain id 4221) from GenLayer Studio
or the `genlayer` CLI, using your own wallet. You need testnet GEN from
https://testnet-faucet.genlayer.foundation/. The explorer is at
https://scan.testnet-chain.genlayer.com.

## Limits

- Only one URL per claim, and the page must be readable as server-side HTML.
  JavaScript-only pages yield `not_found`.
- Dynamic pages that change between leader and validator fetches can fail
  consensus. That is intended: the evidence was not stable.
- The verdict is about what *this page* says, not the truth of the world.
