# Kaen Space App — Nghiên cứu migration từ Kaizen

> Trạng thái: **HOÀN THÀNH cả 4 phase** — 2026-07-20 (Phase 4: `apps/kaen/extension`, contract-test 34/34)
> Nguồn: `/Users/benji/Projects/kaizen` (monorepo học từ vựng SRS)
> Đích: `apps/kaen` (port **4500**, MCP `kaen-mcp` 25 tools, tool prefix `kaen_*`)
>
> Kết quả thực tế: backend Rust 45 unit tests (srs/ops/grammar/llm/story/dictation/dictionary),
> web UI re-port đầy đủ (vocabulary + practice + grammar + story + dictation), AI qua bridge
> `llm.request` đã verify chạy thật (grammar test, story 3 bước). Artifact: `apps/kaen/kaen-app.zip`.
> Khác kế hoạch ban đầu: vite dùng `base: '/'` (không phải `'./'`) vì app serve ở root origin riêng —
> base tương đối làm blank-screen khi hard-refresh nested route.

## 1. Kaizen hiện tại là gì

Monorepo học ngôn ngữ (micro-learning 6 phút + Spaced Repetition), 8 thành phần:

| Thành phần | Stack | LOC | Số phận khi migrate |
|---|---|---|---|
| `backend/` | NestJS 10 + **TypeORM** + PostgreSQL (PRD nói Prisma nhưng thực tế đã chuyển TypeORM) | ~15.8k TS | **Port sang Rust** (chọn lọc) |
| `frontend/` | React 18 + Vite 5 + Tailwind v4 + Zustand + react-router + i18next + PWA | ~51.7k | **Tái sử dụng phần lớn** → `apps/kaen/web` |
| `admin/` | NestJS SSR CMS riêng | ~6.5k | **Bỏ** — single-user không cần CMS |
| `mobile/` | Flutter (Riverpod) | ~20k | **Bỏ** (dự án vệ tinh) |
| `chrome-extension/` + `safari-extension/` | MV3 "Kaen Vocabulary Helper" (tra Cambridge/GTranslate, lưu từ vào lesson) | nhỏ | **Giai đoạn sau** — trỏ API về app local, hoặc nhập từ qua MCP |
| `dailydictation.com/` | Crawler ETL nội dung dictation | — | **Bỏ khỏi runtime** (giữ như tool ngoài) |
| `tools/`, `infastructrure/` | Script + Terraform/K8s/GitLab CI | — | **Bỏ** — Space App không cần deploy hạ tầng |

### Backend modules (25+) — phân loại

**Lõi giá trị (port):**
- `study` (~1.5k dòng) — SRS engine tự chế: sai/từ mới → +30 phút, đúng → level 0-6 với interval cố định `[0.5h, 24h, 72h, 168h, 720h, 2160h]`, snap giờ vào `studySlots` + timezone ("khung giờ vàng"). Không phải SM-2 chuẩn.
- `lesson` / `card` / `saved_lessons` / `user_card_progress` / `review` / `study_logs` — CRUD bài học, thẻ, tiến độ, streak/XP.
- `matching`, `listening`, `writing` — các mode luyện tập.
- `grammar` + `grammar-test` + `user_grammar_progress` — mảng đang phát triển nóng nhất (SRS cho ngữ pháp, sinh test bằng AI, import/export zip).
- `sentence` — phân tích/tạo câu bằng AI.
- `story` — truyện AI theo bài học.
- `dictation-lesson` — chép chính tả (topic → lesson → segment) + pronunciation challenge.
- `dictionary` — từ điển + bản dịch.

**Thay thế bằng hạ tầng SenClaw (không port nguyên bản):**
- `ai-integration`, `ai-content`, `story-ai` + 2 Socket.IO gateway — toàn bộ AI hiện đi qua **Dify** (4 API key, streaming `/chat-messages`). → thay bằng **bridge `llm.request`** của daemon.
- `elevenlabs` (TTS phát âm) — → dùng subsystem TTS sẵn có của SenClaw (`src/tts/`: macos say / mms_vits / vieneu) hoặc bỏ ở v1 (thẻ tiếng Anh: `macos` backend đọc được ngay).
- `notification` + `notification.scheduler` + `mail` — push/email nhắc ôn → thay bằng poller nội bộ + bridge `mcp.call` → `senclaw-send`/schedule (xem §4.5).
- `media-file` — lưu local `uploads/` + ServeStatic → giữ nguyên ý tưởng, chuyển vào `~/.senclaw/space-app-data/kaen/uploads/`.

**Bỏ hẳn:**
- `auth` (JWT + refresh + Google/Facebook OAuth + verify email + reset password), `user` multi-user, `usage-trace`, `report`, `country`, `mail`. Space App là **single-user local, không auth** (convention ghi rõ trong `apps/rewrite-story/src/schema.sql`). Bảng `users` co lại thành `app_settings` (studySlots, timezone, dailyWordGoal, streak, XP, snoozeUntil).

## 2. Khuôn mẫu đích (theo `apps/rewrite-story` — bản port chuẩn nhất)

```
apps/kaen/
  Cargo.toml                # [[bin]] name="kaen"; thêm "apps/kaen" vào workspace members gốc
  senclaw-manifest.json     # id/kind:server/start:./kaen/healthPath:/api/status/port:4500
  src/
    main.rs                 # #![recursion_limit="512"]; axum nest /api + ServeDir SPA
    config.rs               # PORT, SENCLAW_BASE_URL, SENCLAW_SPACE_APP_ID; data_dir()
    db.rs + schema.sql      # rusqlite bundled, Mutex+WAL, CREATE TABLE IF NOT EXISTS
    api.rs                  # REST /api/* (map 1-1 từ NestJS controllers)
    srs.rs                  # thuật toán SRS port từ study.service.ts (+ unit tests golden)
    llm.rs                  # bridge llm.request
    mcp.rs                  # JSON-RPC SSE /api/mcp/sse, tools kaen_*
    process.rs              # poller nhắc ôn (due cards) nền
  web/                      # copy frontend kaizen, vite base:'./', bỏ auth pages
  skills/  personas/  scripts/pack.sh
```

Convention bắt buộc (đã kiểm chứng trong repo):
- **DB ngoài thư mục cài đặt**: `~/.senclaw/space-app-data/kaen/app.sqlite` (cài lại app = `remove_dir_all` thư mục app → DB phải nằm ngoài; `apps/rewrite-story/src/config.rs:34`).
- **SPA fallback bằng `.fallback(spa_index)`**, KHÔNG dùng `not_found_service` (ép 404 làm health-check tưởng app chết; comment `rewrite-story/src/main.rs:60`).
- **Static-dir candidate**: đường dẫn app đứng trước, `web/dist` chung của SenClaw đứng cuối.
- **Bridge LLM giới hạn**: chỉ `system/prompt/maxTokens/profile` — **không có temperature**; `finish=="length"` phải coi là lỗi; trần output cố định → chunk input khi sinh nội dung dài (bài học đã ghi trong memory từ rewrite-story).
- `triggers` của skill khai trong **manifest**, không nằm trong SKILL.md.

## 3. Mapping schema PostgreSQL/TypeORM → SQLite

| Kaizen (PG) | Kaen (SQLite) | Ghi chú |
|---|---|---|
| `users` (26 cột) | `app_settings` (key-value hoặc 1 row) | giữ: study_slots (json), timezone, daily_word_goal, streak, last_study_date, total_xp, snooze_until. Bỏ: email/password/oauth/verification/refresh-token/isBlocked |
| `lessons`, `cards`, `tags` | giữ nguyên, bỏ `ownerId`/`visibility`/`savedCount` | visibility vô nghĩa khi single-user; `saved_lessons` bỏ (mọi lesson đều "của mình") |
| `user_card_progress` | `card_progress` (bỏ userId) | index `(next_review)`; level 0-6, is_urgent |
| `review_sessions`, `study_logs` | giữ (bỏ userId) | nguồn thống kê streak/XP |
| `grammars`, `grammar_topics`, `grammar_questions`, `grammar_test_*`, `user_grammar_progress` | giữ (bỏ userId) | jsonb → TEXT json |
| `user_sentences`, `stories`, `story_steps`, `user_story_progress` | giữ (bỏ userId) | |
| `dictation_*`, `user_dictation_progress` | giữ | nội dung seed từ crawler cũ (import zip/json) |
| `dictionary`, `translation`, `languages` | giữ | có thể ship kèm data seed |
| `notifications`, `review_notifications`, `usage_traces`, `report`, `countries` | **bỏ** | |

Kiểu dữ liệu: uuid → TEXT, jsonb → TEXT (serde_json), timestamptz → TEXT ISO-8601 (UTC) — tính "khung giờ vàng" bằng chrono-tz theo `timezone` trong settings.

## 4. Các quyết định kiến trúc

1. **AI: Dify → bridge.** 4 use-case AI (chat trợ giảng, sinh danh sách từ, sinh story, sinh grammar test) đều map được sang `llm.request` (one-shot, JSON-out + repair kiểu mindmap/truncated-JSON đã có tiền lệ). Chat streaming của `ai-integration` hạ cấp thành request/response ở v1 (bridge không stream); nếu cần agent có tool thì dùng `agent.run`.
2. **TTS:** v1 dùng `src/tts/` backend `macos` cho phát âm từ tiếng Anh (miễn phí, offline). ElevenLabs bỏ.
3. **Auth:** xoá toàn bộ. Frontend bỏ 7 trang auth + OAuthCallback + guard, Zustand auth store thay bằng stub "always logged in".
4. **PWA/service worker:** bỏ `sw.js` khi chạy trong iframe (PWA-SW cache gotcha đã dính ở mindmap); giữ responsive.
5. **Nhắc ôn (thay notification+mail):** `process.rs` poller mỗi phút quét `card_progress WHERE next_review <= now` (tôn trọng `snooze_until`), gửi nhắc qua bridge `mcp.call` → `senclaw-send` về kênh người dùng chọn, hoặc chỉ hiện badge due-count trong UI ở v1. Snooze giữ nguyên logic.
6. **Import nội dung cũ:** viết `kaen_import` (MCP tool + REST) nhận zip/json export từ kaizen (đã có sẵn cơ chế import/export zip cho grammar + import text cho lesson) — không cần migrate trực tiếp từ PostgreSQL.

## 5. MCP tools dự kiến (`kaen-mcp`, prefix `kaen_`)

- Lesson/card: `kaen_lesson_list/create/show`, `kaen_card_add`, `kaen_import_text` (rawText + separator), `kaen_import_zip`
- Study: `kaen_study_session` (sinh phiên 6 phút), `kaen_review_submit` (result REMEMBER/FORGOT + mode), `kaen_due_count`, `kaen_snooze`
- Grammar: `kaen_grammar_list/show`, `kaen_grammar_test_generate` (AI), `kaen_grammar_test_submit`
- Sentence/story: `kaen_sentence_analyze`, `kaen_story_generate`
- Stats/config: `kaen_stats` (streak/XP/log), `kaen_settings_get/set` (studySlots, goal, timezone)

Skills: `kaen-study-coach` (nhắc học, tạo phiên, giải thích từ), `kaen-content-maker` (soạn lesson/grammar/story từ yêu cầu). Persona: `study-coach`.

## 6. Lộ trình đề xuất

- **Phase 1 — Core loop (khung app + SRS):** scaffold theo rewrite-story; schema lesson/card/progress/settings/logs; port `srs.rs` với unit test golden đối chiếu `study.service.ts`; REST study/lesson; web: pages practice + lesson + bank (bỏ auth); MCP study/lesson tools; pack.sh. → app dùng được ngay cho học từ.
- **Phase 2 — Grammar & AI:** grammar + grammar-test (kèm sinh test qua bridge), sentence, import zip, TTS phát âm.
- **Phase 3 — Content mở rộng:** story AI, dictation (+seed data từ crawler), dictionary, matching/listening/writing modes đầy đủ, nhắc ôn qua senclaw-send.
- **Phase 4 (tuỳ chọn):** chrome-extension trỏ về `http://localhost:4500` hoặc lưu từ qua agent/MCC.

Khối lượng: Phase 1 tương đương một app cỡ rewrite-story/moltbook; frontend là phần tiết kiệm lớn nhất (copy + sửa API base + gỡ auth thay vì viết lại).

## 7. Rủi ro / lưu ý

- `study.service.ts` 1.5k dòng nhiều edge case timezone/slot-snapping — cần port kèm bộ test golden (input → nextReview kỳ vọng) trước khi viết Rust.
- Frontend Tailwind **v4** + Vite 5: giữ nguyên toolchain của kaizen trong `apps/kaen/web`, chỉ cần `base:'./'`; không ép về stack Tailwind v3 của web chính.
- Kaizen còn commit `disable api` và nhiều file đang sửa dở (OAuth, grammar-test) — chốt điểm cắt: port theo `main` hiện tại, phần OAuth dở dang bỏ qua (không cần trong Space App).
- Bridge không stream → UX chat AI trong app sẽ là spinner-then-answer; nếu cần streaming thật phải mở rộng bridge (ngoài phạm vi migration).
