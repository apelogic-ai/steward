# mcp-gw verified-claims policy patch

Base: `apelogic-ai/mcp-gw`
`c2af10d9c3dee898e368e6cf3d0f5a1ef6ad0dde`, the merge commit for
[mcp-gw#15](https://github.com/apelogic-ai/mcp-gw/pull/15).

## Why it is carried

The merged introspection support makes an already-issued HOP-1 fail closed
after Steward authority changes. The tool policy input still contains only the
acting user's email, however. It cannot distinguish two runtimes belonging to
the same user and cannot evaluate the runtime-bound tool grants in Steward's
verified HOP-1 claims. Projecting a policy by email would union those runtimes'
authority and make the S1 outside-`spec.tools` negative test vacuous.

This patch adds the already-verified token claims to the generic OPA input for
both wrapper implementations. `mcp-gw` does not interpret Steward's claim
schema; the downstream policy remains responsible for that decision.

## Upstream attempt and exit condition

- [apelogic-ai/mcp-gw#16](https://github.com/apelogic-ai/mcp-gw/pull/16)
  contains this patch at tip
  `34acece3eb59c69d9725d3aa16fadf6b7d072df0`. The original policy-input
  exposure is commit `d373152c4f12c8d5cdb80e9cd5f24cbe8dfece01`; the follow-up
  normalizes the verified email and subject for issuer-independent policy.
- Remove the patch when that PR, or an equivalent verified-claims policy
  interface, lands and Steward pins a release or immutable commit containing
  it.

Apply only to the recorded base:

```bash
git apply third_party/mcp-gw-patches/c2af10d9/0001-expose-verified-claims-to-tool-policy.patch
```

## Verification

The focused negative tests failed before the implementation because the OPA
input omitted `tokenClaims`, then because raw issuer claims did not provide the
normalized identity contract. On the patched tree, `bun run ci` passes:
TypeScript typecheck, ESLint, formatting, and all 193 tests.
