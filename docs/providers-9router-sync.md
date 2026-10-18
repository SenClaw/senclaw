# Providers synced from 9router

SenClaw's provider code was ported from [decolua/9router](https://github.com/decolua/9router) (MIT), a local
OpenAI-compatible router. This file records **which upstream version SenClaw is synced to**, what was taken, what
was left out and why, and how to repeat the sync.

| | |
|---|---|
| First port | v0.5.45 (2026-07-30) |
| Last sync | **v0.5.104 (2026-10-15)**, on 2026-10-19 |
| Ported from | `open-sse/providers/registry/*.js` (the free/freeTier presets), `open-sse/executors/{antigravity,codex}.js`, `src/app/api/providers/[id]/models/route.js` (discovery) |
| SenClaw side | `src/providers/mod.rs` (preset catalog), `src/providers/oauth/` (sign-in, discovery, headers), `src/providers/adapters/{antigravity,codex}.rs` |

## What the 2026-10-19 sync took

- **Preset catalog** (`providers/mod.rs`): new free-tier gateway `agnes`; `api-airforce` models replaced with the current
  free set (`gpt-oss-120b`, `gpt-oss-20b`, `kimi-k2.7-code`); `google-ai-studio` gained `gemini-3.8-flash`,
  `gemini-3.7-flash`, `gemini-4-argon`.
- **Antigravity**: the chat request no longer carries `requestType: "agent"`. Google's own client omits it, and sending it
  makes Code Assist bucket the call and answer a detail-free `429 RESOURCE_EXHAUSTED` with quota to spare (upstream v0.5.85).
  The IDE build reported in the `User-Agent` moved from `1.23.2` to `2.22.0` — Google gates newer models on it.
- **Codex**: tool schemas lose any `pattern` that uses a Unicode property escape (`\p{..}`), which the Codex backend's
  validator rejects with a 400 for every account (upstream #3922); discovery asks for models as client `0.159.0`
  (it was `0.144.6`), because the backend silently drops entries whose `minimal_client_version` is higher.

## What was deliberately not taken

- **Client impersonation.** Upstream spoofs Claude Code (`2.1.x`), Grok CLI and the Codex CLI `User-Agent`, and rewrites
  competing-client branding out of system prompts. SenClaw names itself (`senclaw/<version>`, `originator: senclaw`) and its
  own prompts contain none of that branding; `oauth/transport.rs` has the reasoning. The Antigravity IDE string is the one
  exception already made, because the endpoint is unusable without it.
- **Codex "Responses Lite"** (GPT-6 Sol / Luna and their `[1m]` variants): a different request shape — tools and
  instructions move into `additional_tools` / developer items at the head of `input`, plus the internal header
  `x-openai-internal-codex-responses-lite`. It touches the same identity question as above, so it waits for a decision.
- **Providers that need an adapter, not a preset**: Amazon Bedrock (SigV4 + EventStream), Nous / Meta Muse / MiniMax Code /
  Qoder CN / Xiaomi MiMo OAuth sign-ins, OpenCode Zen (attested free tier), Cursor, Kiro, Vertex.
- **Paid aggregators** (`tokenrouter`, `tokenharbor`, `dahl`, `atria`, `bai`, `alitp-intl`): the catalog holds free tiers only.
- **Router features with no SenClaw counterpart**: combos and fallback, the quota tracker, RTK token compression, proxy
  pools, per-key access control, the CLI-tool configurators, media providers (TTS, image, video, STT, web search).
- **Antigravity details** not ported yet: the per-session thought-signature cache and the "only the first parallel call is
  signed" rule (SenClaw backfills the default signature on every call and has a test that says so), and parsing
  `quotaResetTimeStamp` / `retryDelay` out of a 429.

## How to repeat the sync

```bash
git clone --depth 1 https://github.com/decolua/9router.git /tmp/9router-new      # read it, never run it
# what changed in the provider registry since the last synced version:
diff -rq <old-checkout>/open-sse/providers/registry /tmp/9router-new/open-sse/providers/registry
# the three files SenClaw ports from:
diff <old>/open-sse/executors/antigravity.js /tmp/9router-new/open-sse/executors/antigravity.js
diff <old>/open-sse/executors/codex.js       /tmp/9router-new/open-sse/executors/codex.js
diff "<old>/src/app/api/providers/[id]/models/route.js" "/tmp/9router-new/src/app/api/providers/[id]/models/route.js"
```

The changelog (`CHANGELOG.md`, newest first) says why each change happened. Take what fits SenClaw's rules above, update the
table at the top of this file, and keep the tests that pin the behaviour (`providers::` in `cargo test --lib`).
