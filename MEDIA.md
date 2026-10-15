# Media APIs and OmniRoute audit

SparkRouter supports image, video, music, text-to-speech (TTS), and
speech-to-text (STT) requests through the same gateway authentication, exact
public-model scopes, per-key traffic limits, and combo/direct routing as chat.
**A provider has one card and one set of connections**, not separate Text,
Image, Video, Music, or Speech provider entries.

## Implemented endpoints

| Public endpoint | Input | Output |
| --- | --- | --- |
| `POST /v1/images/generations` | JSON with `model`, `prompt`, upstream options | OpenAI-style `created` / `data` with URLs or `b64_json` |
| `POST /v1/images/edits` | Multipart `model`, `prompt`, `image`/`image[]`, optional `mask`, upstream options | OpenAI-compatible image response; compatible providers only |
| `POST /v1/videos/generations` | JSON with `model`, `prompt`, upstream options | `created` / `data` with video URLs; synchronous gateway response |
| `POST /v1/music/generations` | JSON with `model`, prompt/lyrics/tags/reference audio/composition plan and upstream options | `created` / `data` with URLs or base64; **ElevenLabs returns binary audio** |
| `POST /v1/audio/speech` | JSON with `model`, `input`, optional `voice`, `response_format`, upstream options | Binary audio |
| `POST /v1/audio/transcriptions` | Multipart `model`, `file`, upstream options | JSON with `text`, or a supported upstream text/subtitle format |
| `POST /v1/audio/translations` | Multipart `model`, `file`, upstream options | Compatible upstream transcription/translation response |

Use `combo/<name>` or a provider-prefixed ID from `/v1/models`. A combo can
reference any upstream ID, including one absent from discovery. Direct calls
still require a saved model-list snapshot; there are no raw-ID aliases.

### Supported integrations

| Provider/protocol | Implemented capabilities | Implementation |
| --- | --- | --- |
| OpenAI | Text, Image, TTS, STT | Existing provider and Bearer key; native compatible image/audio paths |
| Groq | Text, TTS, STT | Existing provider; compatible audio paths |
| Together | Text, Image, TTS, STT | Existing provider; compatible image/audio paths (model-dependent) |
| xAI | Text, Image, Video | Existing provider; compatible image path and native video submit/poll |
| MiniMax / MiniMax China | Text, Image, Video, Music, TTS, STT | Existing cards/keys; native image/music/TTS/STT and Hailuo video submit/poll |
| fal.ai | Image, Video, Music | One new provider; native queue submit/poll (or a custom direct endpoint), using `Authorization: Key` |
| Deepgram | TTS, STT | One new provider; native `speak` / binary `listen`, `Authorization: Token` |
| ElevenLabs | Music, TTS, STT | One new provider; native music, voice-specific TTS, multipart STT, `xi-api-key` |
| Custom OpenAI-compatible | Image, Image edits, Video, Music, TTS, STT, Translation | Compatible paths relative to the configured API base; availability depends on the upstream |

Other existing OpenAI-compatible presets can use these routes **only if their
upstream implements the corresponding paths and response contract**. A Text
model is not automatically an image/audio/video model. Combos have an enforced
category; mixed categories and wrong-mode requests are rejected, not silently
filtered. Protocol compatibility and explicit modality metadata are checked;
unlisted IDs are manually declared in the combo's category when added. Selected
exact vendor IDs also have built-in categories, and explicit discovered endpoint
lists constrain classification. These are advisory capability facts, not account
access checks; no broad model-name heuristics are used. See
[category-specific combos](README.md#category-specific-combos) for policy and
provider scoping. Capability tags are editable display
metadata, not routing permissions or proof of account access.

Custom providers can select OpenAI, MiniMax, xAI, fal.ai, Deepgram, or ElevenLabs
protocols. Existing MiniMax and xAI presets retain their original OpenAI protocol
and identities; their documented hosts select the native media adapters without
changing keys, membership, slugs, or combo references. Custom mirrors should
select the explicit vendor protocol, since arbitrary hosts are not inferred.
Base URLs include the API prefix: `/v1` for speech vendors/MiniMax/xAI, and
`https://queue.fal.run` for the fal.ai preset. Custom fal.ai providers can use
`https://fal.run` for lower-overhead synchronous inference; long-running work is
better served by the queue endpoint. All calls use the shared redirect-disabled client.

### Availability and discovery

- **OpenAI video is not advertised.** Its official Videos API reference says the
  Sora API shut down on September 24, 2026. `/videos/generations` is not a
  claim of support for OpenAI's retired `/videos` job API.
- **MiniMax music is account-dependent.** Its documentation says that from
  August 20, 2026 paid music APIs are closed to new users and free APIs are being
  discontinued. The adapter remains useful for eligible accounts and mirrors.
- MiniMax H3's newer V2 video API is **not** the Hailuo V1 task API implemented
  here. Select a compatible Hailuo model, not H3.
- Deepgram and ElevenLabs refresh their native `/models` catalogs. MiniMax uses
  `/models`, supplemented with documented media IDs. ElevenLabs refreshes also
  include documented music/STT IDs that may be absent from its voice model list.
- fal.ai refresh installs a **small documented starter catalog**, not a live
  account query. Model details label this source. This does not validate the key
  or prove model access. Its connection probe reports that a generation test is
  needed instead of treating a nonexistent `/models` route as connectivity.
- If an existing custom provider already owns a newly added preset's routing
  slug, creating an ungrouped preset connection is rejected rather than merging
  namespaces and silently broadening key scopes. Use an explicit custom media
  provider with a distinct slug; existing group IDs/slugs are not rewritten.
- Arbitrary model-specific fields are retained for compatible/Fal requests.
  Native adapters translate the relevant envelope fields. For fal ACE-Step,
  `prompt` maps to `tags` if `tags` was not supplied; other fal model inputs should
  use their documented field names. IDs include the full `fal-ai/…` suffix.
- ElevenLabs voices are real upstream voice IDs, not guessed OpenAI voice aliases.
  MiniMax accepts `voice` as a native voice ID or explicit `voice_setting`.
  Deepgram uses the selected model's voice. Deepgram/ElevenLabs STT support JSON
  and plain text here; unsupported output formats are rejected before submission.

## Examples

Create combos named `images`, `videos`, `music`, `speech`, and `transcribe`, with
categories **Image**, **Video**, **Music**, **TTS**, and **STT**, respectively, and
steps pointing to the appropriate models. Commands below make billable requests.

```sh
curl http://127.0.0.1:14673/v1/images/generations \
  -H "Authorization: Bearer $ROUTER_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"combo/images","prompt":"A watercolor mountain landscape"}'

curl http://127.0.0.1:14673/v1/videos/generations \
  -H "Authorization: Bearer $ROUTER_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"combo/videos","prompt":"Clouds moving over mountains","duration":6}'

curl http://127.0.0.1:14673/v1/music/generations \
  -H "Authorization: Bearer $ROUTER_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"combo/music","prompt":"Gentle instrumental ambient music"}'

curl http://127.0.0.1:14673/v1/audio/speech \
  -H "Authorization: Bearer $ROUTER_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"combo/speech","input":"Hello world","voice":"alloy"}' \
  --output speech.mp3

curl http://127.0.0.1:14673/v1/audio/transcriptions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -F model=combo/transcribe -F file=@speech.mp3
```

Use a native voice ID instead of `alloy` for ElevenLabs or MiniMax. MiniMax music
may require lyrics or `is_instrumental: true`; pass the model's native options.
The dashboard **Test** page (`/test`) also offers all seven media modes. Select
a compatible combo, enter a prompt/speech text or upload a file, and optionally
supply a mask/voice plus native fields in **Advanced media options (JSON)**.
Inline images and binary audio support local preview/download; remote URLs are
explicit no-referrer links, never automatically fetched. Video may take several
minutes. Inputs are retained for deliberate repeats, but results/files/keys are
never saved in browser storage; media request history remains metadata-only.
Leaving the page does not guarantee upstream cancellation or prevent charges.

## Performance, limits, and failure semantics

- Reuse one pooled HTTP client. No SDK runtimes, transcoding, background workers,
  or external queues/databases were added. Binary speech and ElevenLabs music
  are streamed with backpressure; JSON/hex upstreams are bounded and buffered.
- Incoming JSON remains capped at **4 MiB**. Multipart media requests are capped
  at **32 MiB including framing**, **64 fields**, and **64 KiB per text field**.
  Audio uses exactly one file. Replayable upload buffers are shared by reference
  across ordered attempts. There is no unbounded filesystem spool.
- JSON responses are capped at **16 MiB**; binary output at **64 MiB**. Empty or
  wrong-content-type binary responses can fail over before commitment. Once
  committed, interruption/overflow terminates the body, never switches targets.
- Media preparation has a **600-second per-attempt** and **610-second total**
  deadline; the shared **120-second idle read timeout** still applies. Native
  xAI/MiniMax video and fal queue polling is inline every two seconds with a
  570-second poll budget. They return a completed URL or a sanitized failure, not a resumable
  gateway job. Cancelling a request stops gateway polling but may not cancel an
  already accepted upstream job; it can still finish and incur charges.
- Explicit rejection of a native job submission can use ordered fallback.
  Once submission is accepted (including malformed success bodies), or transport
  failure makes acceptance ambiguous, do not submit a second billable job.
  Polling/result failures cannot re-enter fallback. fal successful submissions
  use the same conservative no-backup policy, including malformed results. fal
  poll/result URLs must stay on the configured origin and base path and refer to
  the submitted request ID; credentials are never sent to a returned foreign URL.
  Generic compatible routes require completed outputs: HTTP 202 or a recognized
  pending job envelope returns a sanitized failure without creating a backup job;
  there is no generic gateway resume endpoint.
  Other synchronous calls use
  the normal pre-commit retry policy; ambiguous transport failures can still
  cause duplicate upstream work, as with chat requests.
- Generated-key concurrency permits cover polling and the full lifetime of
  streamed binary bodies, including cancellation. Affinity is endpoint-isolated;
  binary output establishes affinity only on successful EOF.
- Media history is **metadata-only even in Extended logging**. Uploaded files,
  prompts, output bodies, and signed media URLs are not captured. Usage remains
  unknown when not supplied; no fabricated token/cost counts. Binary history
  completion is based on clean EOF, not an SSE `[DONE]` marker.
- Returned media URLs are passed to the caller; SparkRouter does **not** fetch
  arbitrary output/image/audio URLs or forward API keys to a CDN. Remote inputs
  in native JSON remain the upstream's responsibility. Benchmarking against real
  providers has not been performed; these are bounded low-copy design choices,
  not a measured claim of being faster than OmniRoute.

## OmniRoute source audit

Inspected the local checkout's `open-sse/config/{imageRegistryData,audioRegistry,
videoRegistry,musicRegistry}.ts`, generation handlers, native media handlers,
and shared media helpers. OmniRoute combines compatible proxying, provider-native
translations, submit/poll/fetch workflows, local workflow engines, cloud/OAuth
credentials, and web-session integrations. A registry row alone does not prove
an implementation is callable or currently offered.

In addition to the integrations above, its registries/handlers include these
families, **not implemented natively in this change**:

- Image: Stability AI, Black Forest Labs, Google Imagen/Gemini/Vertex, Ideogram,
  Leonardo, Haiper, Segmind, Nvidia NIM, Cloudflare, Alibaba/Qwen, Replicate,
  Hyperbolic, AI Horde, ComfyUI/SD WebUI, Adobe Firefly, KIE, and other gateways.
- Video: Google Vertex/Flow, Runway, Replicate, Together's distinct video job API,
  DeepInfra, Alibaba/Qwen/DashScope, Novita, Leonardo, Haiper, Segmind,
  ComfyUI/SD WebUI, Adobe Firefly, KIE, Syntx, Agnes, and web-session services.
- Music: Vertex Lyria, KIE/Suno, Udio web sessions, ComfyUI, Syntx.
- Speech: AssemblyAI, Soniox, Gladia, Rev AI, Speechmatics, Nvidia NIM,
  Google/Vertex, AWS Polly, Cartesia, Fish Audio, PlayHT, Inworld, Hyperbolic,
  Qwen, Coqui/Tortoise, gTTS, and other hosted/web integrations.

These need their own documented contracts, credentials, output normalization,
job semantics, and local regression tests. They are deliberately not mislabeled
as supported merely because OmniRoute has a registry entry. Compatible gateways
in those families can already work through the custom OpenAI media protocol.
No OmniRoute web-cookie/OAuth scraping or guessed ComfyUI workflows were copied.

### Independent upstream references

Protocol decisions were cross-checked against upstream documentation, not just
OmniRoute code:

- [OpenAI Audio / Speech](https://developers.openai.com/api/reference/resources/audio/subresources/speech/methods/create)
- [OpenAI Transcriptions](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
- [OpenAI Videos reference and deprecation](https://developers.openai.com/api/reference/resources/videos/methods/create)
- [fal direct/subscribe inference](https://fal.ai/docs/documentation/model-apis/inference/synchronous)
- [fal Queue API](https://fal.ai/docs/documentation/model-apis/inference/queue)
- [fal Model APIs](https://fal.ai/docs/documentation/model-apis/overview)
- [MiniMax Music](https://platform.minimax.io/docs/api-reference/music-generation)
- [MiniMax image API](https://platform.minimax.io/docs/api-reference/image-generation-t2i)
- [MiniMax TTS](https://platform.minimax.io/docs/api-reference/speech-t2a-http)
- [MiniMax STT](https://platform.minimax.io/docs/api-reference/speech-to-text)
- [MiniMax video task status](https://platform.minimax.io/docs/api-reference/video-generation-query)
- [MiniMax error codes](https://platform.minimax.io/docs/api-reference/errorcode)
- [xAI video](https://docs.x.ai/developers/model-capabilities/video/generation)
- [Deepgram speech](https://developers.deepgram.com/reference/text-to-speech/speak-request)
- [Deepgram native model listing](https://developers.deepgram.com/reference/manage/models/list)
- [ElevenLabs TTS](https://elevenlabs.io/docs/api-reference/text-to-speech/convert)
- [ElevenLabs STT](https://elevenlabs.io/docs/api-reference/speech-to-text/convert)
- [Together speech](https://docs.together.ai/reference/audio-speech)
- [Together transcription](https://docs.together.ai/reference/audio-transcriptions)
- [ElevenLabs models](https://elevenlabs.io/docs/api-reference/models/list)
- [ElevenLabs music](https://elevenlabs.io/docs/api-reference/music/compose)
