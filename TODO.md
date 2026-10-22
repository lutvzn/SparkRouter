# Cache-aware routing TODO

Assessment of the cache-aware routing ideas described for OmniRoute against
SparkRouter's current implementation. These are candidate improvements, not a
commitment to reproduce OmniRoute's wider feature set.

## Existing baseline

SparkRouter already implements:

- Basic/Smart explicit-session affinity and success-based target stickiness.
- Exact combo step, connection, and model locality, rather than provider-only pins.
- Smart conversation-start fingerprints, including initial instructions, the first
  user message, and tool definitions, with endpoint-specific handling.
- Per-combo affinity timeouts and a bounded, process-local map of up to 4096 hints.
- Caller/combo/endpoint isolation and keyed fingerprints instead of retained raw
  session IDs or prompts.
- Retryable-failure hint invalidation and optional failing-target cooldown.
- Affinity updates only after successful stream completion; no fallback after
  streaming commitment.
- OpenAI request pass-through and preservation of Anthropic `cache_control`
  breakpoints on translated content, messages, and tools.
- Cached-input token telemetry and routing diagnostics in History, with
  prompt-cache summaries by combo on Overview.
- Connection preference for known Responses `previous_response_id` continuations.

Relevant implementation: `src/routing/affinity.rs`, `src/routing/combo.rs`,
`src/routing/direct.rs`, and `src/providers/anthropic.rs`. Existing affinity
regression coverage lives in `tests/combo_affinity.rs`.

Two important distinctions:

- Smart is conversation-start affinity, not shared reusable-prefix affinity.
  Different first user questions produce different fingerprints even when their
  system instructions and tools match.
- Combo affinity already pins the exact model and connection together. A separate
  session-to-model table would mostly duplicate that unless it introduced a
  distinct lifetime or policy.

## Candidate improvements, in priority order

### 1. Optional direct-route session affinity

- [x] Assess demand from clients using direct model IDs with multiple connections.
- [x] Add success-based affinity for direct routes (2026-10-21, at the owner's
  request ahead of migrating from OmniRoute): Settings → Routing →
  Direct-route affinity, 5 minutes by default, 0 for off. One hint per caller
  scope, public model ID, and endpoint (no per-session split; oldest-first
  already co-locates sessions). See `tests/direct_affinity.rs`.

Direct routes currently try eligible connections within the provider oldest
first, subject to cooldown and stored-response continuation preferences. They do
not use combo session affinity. If A fails and B succeeds, the next ordinary
request starts at A again rather than remembering B.

Suggested constraints:

- Scope hints by caller namespace, public model ID, endpoint, and session
  fingerprint.
- Reuse the bounded, ephemeral approach rather than adding persistent bindings.
- Never cross provider slugs or bypass refreshed-model eligibility.
- Preserve once-each fallback, permanent-error handling, cooldown behavior, and
  streaming commitment guarantees.
- Keep the feature opt-in: oldest-first is the current direct-routing contract.
- Add local-mock regression tests and update direct-routing documentation.

### 2. Optional strict-priority preservation

- [ ] Decide whether operators need a strict-priority policy alongside current
  success-sticky behavior.
- [ ] If justified, make the policy explicit without changing existing defaults.

Today, enabled combo affinity can override Priority's starting target:

```text
Configured: A → B → C
A fails; B succeeds
Next matching request: B → C → A
```

That is intentional success-stickiness, not a defect. Some operators may instead
want A tried first on every request.

Possible approaches:

- Preserve the declared inter-model priority order.
- Limit affinity to interchangeable connections for the same provider/model.

Define the interaction with cooldown explicitly: cooldown independently permits
reordering. Preserve existing Basic/Smart/Sticky behavior unless the operator
selects a new policy. Cover ordering and fallback with local-mock tests.

### 3. Shared-prefix rendezvous placement

- [ ] Use workload evidence and existing cache telemetry to assess whether shared
  prefixes justify a new placement policy.
- [ ] If justified, add deterministic prefix placement within an explicitly
  permitted routing scope.

Unlike Smart conversation fingerprints, reusable-prefix affinity could place
requests with identical system instructions and tools together even when their
first user messages differ.

Potential benefits:

- Concurrent cold requests agree on a destination before any success is learned.
- Independent conversations can reuse placement for a large shared prefix.
- No per-prefix database binding is required.

Design constraints:

- Prefer same-provider/model connection selection first. Reordering across models
  changes answer behavior and must require an explicit policy or strategy.
- Include the concrete connection and upstream model in target identity.
- Define precedence between session pins and deterministic prefix placement;
  avoid applying two independent affinity decisions to the same choice.
- Preserve caller isolation and retain only keyed fingerprints, not raw prefixes.
- Avoid deriving a common key from an empty or unusable prefix.
- Account for traffic concentration when many requests share a generic prefix.
- Preserve enabled-target eligibility, cooldown, once-each fallback, and streaming
  guarantees.
- Document that deterministic placement does not prove an upstream cache hit or
  reduced cost.

### 4. Capability-gated automatic `prompt_cache_key`

- [ ] Verify against real traffic that generated keys raise OpenAI cache hits
  (not yet measured; no billable requests were made).
- [x] Generate a missing key from the caller and the shared prefix (instructions,
  leading system/developer items, tools) for `api.openai.com` connections only
  (2026-10-21). Always on there; no Settings switch.

SparkRouter already preserves client-supplied `prompt_cache_key` and can use it
as an affinity identity. It does not generate one automatically.

Constraints:

- Never overwrite a client-supplied key.
- Do not infer support merely from an OpenAI-compatible protocol label; some
  compatible endpoints reject unsupported fields.
- Keep session identity and reusable-prefix identity conceptually separate.
- Do not inject fields universally or rewrite stable prompt/tool content.
- Add capability-specific tests and documentation.

## Defer unless separately requested

| Feature | Reason to defer |
| --- | --- |
| Persistent session-affinity database | Adds retention, cleanup, and invalidation complexity; bounded ephemeral hints fit the current scope. |
| Quota-headroom / OAuth availability scoring | Requires reliable provider-specific account state that SparkRouter does not currently maintain; cooldown is the simpler existing mechanism. |
| Automatic quality/cost/latency/cache scoring | Substantially broader than ordered combo routing. |
| Separate session-model history | Mostly duplicates current exact-target combo hints. |
| Cache-aware compression | SparkRouter does not compress prompts; keeping that simplicity avoids prefix-rewriting problems. |
| Local exact-response caching | A separate feature requiring rules for tools, stateful responses, privacy, nondeterminism, and accounting—not a prerequisite for upstream cache affinity. |
| Semantic response caching | Greater correctness risk and complexity; poor fit for the current scope. |

## Overall recommendation

Do not introduce a new cache subsystem without a demonstrated need. The existing
Basic/Smart/Sticky modes, cooldown, marker preservation, and telemetry provide the
practical baseline.

The largest conceptual gap is shared-prefix placement. The most practical routing
coverage gap is affinity for direct model IDs. Implement only the extensions
supported by actual usage, keeping routing policy explicit and correctness ahead
of cache locality.
