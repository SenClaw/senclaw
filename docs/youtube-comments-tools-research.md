# Nghiên cứu bổ sung: công cụ bình luận, phân tích thống kê & kết nối nền tảng

> Bổ sung cho [`docs/youtube-app-research.md`](youtube-app-research.md). Trạng thái: **RESEARCH / PLAN** — chưa implement. Ngày: 2026-07-21. Mục tiêu: mở rộng `apps/youtube` quanh **bình luận** theo 3 hướng — (A) công cụ thao tác, (B) phân tích thống kê, (C) kết nối các nền tảng khác trong SenClaw.

---

## 0. TL;DR + khuyến nghị

- **Phân tích bình luận PHẢI tính tại app.** YouTube Analytics API chỉ có `commentCount` tổng — **không có metric per-comment, không sentiment**. Mọi thống kê (cảm xúc, ý định, chủ đề, spam) đều dùng LLM của SenClaw trên comment kéo về + cache cục bộ.
- **Kiểm duyệt có 2 đường, chọn theo quyền:** InnerTube (like/heart/pin/remove/report — hợp với extension ta đang có) vs Data API `comments.setModerationStatus` (held/reject/ban — chính thức nhưng **cần OAuth chủ kênh**).
- **Kết nối CRM: KHÔNG push, mà CRM PULL.** Chỉ cần app YouTube expose `GET /api/inbox?since=` + `POST /api/inbox/reply` + `GET /api/status` giống hệt `apps/social`, rồi operator thêm một CRM channel kind `social` trỏ về port 4491 → **0 dòng sửa CRM**.
- **Khuyến nghị thứ tự làm:** (1) cache comment + sync → (2) phân tích LLM + dashboard → (3) kết nối CRM (pull-feed) → (4) knowledge + cảnh báo từ khoá. Moderation (Data API/OAuth) để sau vì cần luồng OAuth riêng.

---

## 1. YouTube cho bình luận: làm được gì

### 1.1 Ba tầng API

| Tầng | Đọc | Ghi/hành động | Auth | Ghi chú |
|---|---|---|---|---|
| **InnerTube** (`youtubei/v1`) | `next` (comment threads + replies, phân trang) | `comment/perform_comment_action`: like/dislike/**creatorHeart**/pin/remove/**report** — action-token lấy từ chính `commentRenderer.actionButtons` | cookie phiên (đã có qua extension) | Đường ta đang dùng; không quota |
| **Data API v3** | `commentThreads.list`, `comments.list` | `comments.insert/update/delete`, **`comments.setModerationStatus`** (held/published/rejected + `banAuthor`) | **OAuth `youtube.force-ssl`, chủ kênh/video** | Đường chính thức cho moderation hàng loạt |
| **Analytics API** | metric tổng hợp (`comments`, `likes`, `shares`…) | — | OAuth | **KHÔNG per-comment, KHÔNG sentiment** |

→ Hệ quả thiết kế: **đọc + reply + heart/pin/remove** đi qua InnerTube (đã có hạ tầng); **moderation held/reject/ban chuẩn** cần thêm OAuth (Phase sau); **mọi phân tích** tự tính tại app.

### 1.2 `perform_comment_action` — cần action-token
Mỗi comment renderer chứa sẵn token cho từng hành động (heart, remove, report…). Parser hiện tại (`innertube::parse_comments`) mới lấy `replyParams`; cần mở rộng để lấy thêm các action-token này (đã có helper `find_key_str` để trích).

---

## 2. Nhóm A — Công cụ bình luận bổ sung

Thêm vào `mcp.rs` (đặt tên `mcp__youtube-mcp__youtube_*`), gọi qua `youtube.rs` → InnerTube proxy đã có.

| Tool | Việc | Đường | Guardrail |
|---|---|---|---|
| `youtube_comment_replies` | Tải các reply của một comment (phân trang) | InnerTube `next` + continuation của reply | đọc, free |
| `youtube_sync_comments` | Kéo toàn bộ comment của video → **cache vào DB** (nền cho phân tích + feed) | InnerTube `next` lặp continuation | đọc, throttle |
| `youtube_comment_action` | `heart` / `unheart` / `pin` / `like` trên comment của video mình | `perform_comment_action` + action-token | ghi → draft/approve nếu là hành động công khai; heart/pin thì nhẹ, có thể confirm 1 bước |
| `youtube_comment_remove` | Xoá/ẩn comment trên video mình | `perform_comment_action` (removeComment) | **ghi nguy hiểm → approve bắt buộc** |
| `youtube_comment_report` | Báo cáo comment spam/lạm dụng | `perform_comment_action` (reportComment) | approve |
| `youtube_moderate` *(Phase OAuth)* | `heldForReview` / `rejected` / `banAuthor` hàng loạt | Data API `comments.setModerationStatus` | approve + cần OAuth chủ kênh |

Tất cả hành động ghi đi qua **cùng pipeline draft→approve→send + throttle 30s** đã có; heart/pin có thể là "quick action" chỉ confirm 1 lần vì ít rủi ro.

---

## 3. Nhóm B — Phân tích thống kê

### 3.1 Tầng cache (bắt buộc — vì Analytics API không có)
Thêm bảng vào `db.rs`:
```sql
comments(id TEXT PK, video_id, author, author_channel, text, like_count,
         reply_count, published_at, parent_id, fetched_at)
comment_analysis(comment_id PK, sentiment, intent, topics_json, lang,
                 is_spam, toxicity, analyzed_at, model)
```
`youtube_sync_comments` đổ vào `comments`; một job phân tích đổ vào `comment_analysis`.

### 3.2 Phân tích bằng LLM (bridge `llm.request`)
Chạy theo lô (cap chunk theo trần output đã biết của bridge), mỗi comment →:
- **sentiment**: pos / neu / neg (+ điểm)
- **intent**: câu hỏi · khiếu nại · khen · góp ý · spam · off-topic
- **topics**: 1–3 nhãn chủ đề
- **lang**: ngôn ngữ
- **spam/toxicity**: cờ + điểm

Prompt trả JSON, parse bằng bộ `repair_truncated_json` đã có sẵn ở app khác (mindmap) — port sang.

### 3.3 Dashboard động (theo đúng khuôn CRM)
Sao chép mẫu **`apps/crm/src/db_dashboard.rs`** — registry `&'static str` compile ra SQL an toàn (user chỉ *chọn* field/metric, mọi value là bound param):
- **element** `comment`; **metrics**: `count`, `avgSentiment`, `avgLength`, `spamRate`; **fields**: `video`(relation), `author`, `sentiment`(enum), `intent`(enum), `lang`(enum), `published_at`(date).
- Tools: `youtube_query { element, metric, grouping, filters }`, `youtube_dashboard_schema`, `youtube_create_chart`, `youtube_list_charts` — y hệt `crm_query`/`crm_create_chart`.
- REST `/api/dashboard/{schema,charts,preview,values}` — copy `apps/crm/src/api_dashboard.rs`.
- Web: một tab "Thống kê" render lưới chart (theo `crm` dynamic dashboard).

### 3.4 Chỉ số tổng hợp (ngoài chart)
- **Velocity**: comment/giờ, phát hiện đợt tăng đột biến.
- **Top authors / repeat commenters**, **reply-rate** (đã trả lời / tổng).
- **Hàng đợi câu hỏi chưa trả lời** (`intent=question` AND chưa có reply của mình) — nối thẳng sang CRM/draft.

---

## 4. Nhóm C — Kết nối nền tảng (điểm nối THẬT trong repo)

### 4.1 CRM inbox — **CRM kéo, không đẩy** ⭐
`apps/crm` không có endpoint nhận-đẩy; nó **poll** qua adapter. `apps/social` đã làm sẵn khuôn:
- CRM `channels/social.rs`: mỗi 15s `GET {base_url}/api/inbox?since={cursor}` đọc `messages[{id,platform,external_id,sender,text}]`, reply qua `POST {base_url}/api/inbox/reply {platform,external_id,text}`, health `GET /api/status`.
- **App YouTube chỉ cần expose 3 route đó** (map comment → `{id, platform:"youtube", external_id:"youtube:{commentId}", sender:author, text}`; `reply` → pipeline draft/send). Rồi operator thêm CRM channel **kind `social`** với `base_url=http://127.0.0.1:4491`. → **0 sửa CRM**.
- Muốn "youtube" hạng nhất: thêm `"youtube"` vào `CHANNEL_KINDS` (`apps/crm/src/db.rs:757`) + `channels/youtube.rs` (copy `social.rs`). Chỉ ~1 sửa nhỏ ở CRM.
- Lợi ích: câu hỏi/khiếu nại từ bình luận thành hội thoại CRM, gắn được vào khách hàng (`resolve_customer` theo external_id), có SLA/handoff của CRM.

### 4.2 Knowledge base (bridge `knowledge.*`)
`SpaceClient.knowledge_save(text, Some("youtube-comments"), source)` — space mặc định = app id, có thể đặt riêng. Dùng để:
- Index nội dung comment + câu hỏi hay → `knowledge_recall(q, "youtube-comments")` gợi ý câu trả lời cho `youtube_draft_comment` (trả lời có ngữ cảnh, giảm bịa).
- Xây FAQ tự động từ cụm câu hỏi lặp.
- (SDK chưa expose `tags/mode/hops` → thêm wrapper mỏng gọi `bridge_action` trực tiếp nếu cần gắn tag theo video.)

### 4.3 App-to-app MCP
`mcp.call` và `space.rest` của bridge **đang là stub** — đừng dùng. Cách chạy được: copy **`apps/search/src/transport/app_mcp.rs`** (~260 dòng, self-contained): discover qua `GET {daemon}/api/space/apps` → `POST {origin}/api/mcp/message {tools/call}`. Ví dụ:
- Gọi `crm_query`/`crm_create_chart` để đẩy số liệu bình luận vào dashboard CRM.
- Khiếu nại nặng → tạo task Kanban (`apps/kanban` MCP) cho đội xử lý.

### 4.4 Scheduler / cảnh báo từ khoá
Không có bridge action cho schedule/notify. Dùng REST daemon **`POST /api/background/tasks`**:
- `trigger_type:"interval"` (ms, vd `1800000`=30') + `notify:true` → OS notification khi có bình luận khớp từ khoá (prompt = nội dung cảnh báo).
- Hoặc task chạy agent gọi `youtube_sync_comments` + phân tích định kỳ.
- Caveat: task tạo qua REST là `owner_kind:User` (không phải App-owned) — chấp nhận được cho cảnh báo.

### 4.5 Sơ đồ luồng

```
Bình luận (InnerTube qua extension)
      │  youtube_sync_comments
      ▼
  DB cache (comments)
      │  phân tích LLM (bridge llm.request)
      ▼
  comment_analysis ──► Dashboard động (youtube_query / /api/dashboard)     [B]
      │
      ├─► Feed /api/inbox?since= ──► CRM (channel "social" PULL) ──► hội thoại/khách hàng   [C.1]
      ├─► knowledge_save(space="youtube-comments") ──► recall gợi ý trả lời                  [C.2]
      ├─► app_mcp → crm_query / kanban_create (khiếu nại nặng)                                [C.3]
      └─► background task interval+notify (cảnh báo từ khoá)                                  [C.4]
```

---

## 5. Khả thi ngay vs cần thêm điều kiện

| Hạng mục | Khả thi với hạ tầng hiện có | Cần thêm |
|---|---|---|
| Đọc comment + replies, cache | ✅ InnerTube + DB | — |
| Phân tích LLM (sentiment/intent/topic/spam) | ✅ bridge llm.request | — |
| Dashboard động | ✅ copy `db_dashboard.rs` | — |
| Reply / heart / pin / remove / report | ✅ InnerTube `perform_comment_action` | verify token shape với phiên thật |
| CRM inbox (pull feed) | ✅ expose 3 route + operator thêm channel | (tùy chọn) thêm kind `youtube` |
| Knowledge recall | ✅ SpaceClient | — |
| App-to-app (crm/kanban) | ✅ copy `app_mcp.rs` | — |
| Cảnh báo từ khoá | ✅ daemon `/api/background/tasks` | task là User-owned |
| **Moderation held/reject/ban chuẩn** | ❌ | **OAuth chủ kênh + Data API** |

---

## 6. Roadmap (nối tiếp P1–P5 đã xong)

- **P6 — Cache + sync** ✅ (2026-07-21, 16 test pass): bảng `comments`/`comment_analysis` (db.rs); `youtube::sync_comments` phân trang continuation → upsert idempotent (báo `new` vs cập nhật); tool `youtube_sync_comments` + `youtube_cached_comments` + REST `/api/comments/sync|cached`; parser `parse_comments` mở rộng (likeCount/authorChannel/published + best-effort `heartToken`/`likeToken`) + `find_next_continuation`. Verify: upsert-idempotent + sync-paging-dedupe qua harness giả lập.
- **P7 — Phân tích + dashboard** ✅: `llm::analyze_batch` (LLM chấm sentiment/intent/topic/spam/lang, JSON array, parser chịu fence/chatty); `youtube::analyze_pending` theo lô 15 (lưu placeholder cho id model bỏ sót → không lặp vô hạn); `db::comment_stats` (thay vì port full `db_dashboard.rs` — chọn stats cố định gọn hơn) → tools `youtube_analyze_comments`/`youtube_comment_stats` + `/api/comments/{analyze,stats}` + **tab web "Bình luận & thống kê"** (bar breakdown).
- **P8 — Hành động comment** ✅ (+moderation): `youtube_comment_action` heart/like/pin (reversible) + **remove/report** (destructive, cổng `confirm=true` chặn trước khi tra token) qua `comment/perform_comment_action`; token remove/report lấy từ overflow-menu theo `icon.iconType` (DELETE→remove, FLAG→report, KEEP→pin) trong `parse_comments` + `find_menu_token`; throttle. Moderation qua InnerTube (không cần OAuth) — chỉ chủ kênh mới có token remove/pin.
- **P9 — CRM pull-feed** ✅: `GET /api/inbox?since=` (cursor = rowid) + `POST /api/inbox/reply` (map external_id=commentId → replyParams cache → `send_action reply`) + `/api/status`. Operator thêm CRM channel kind `social` base_url=4491 → 0 sửa CRM.
- **P10 — Knowledge + cảnh báo** ✅: `youtube_index_comments` → `knowledge_save(space="youtube-comments")`; `youtube_scan_keywords` (nguồn dữ liệu cảnh báo; lịch đặt qua daemon `/api/background/tasks`). App-to-app cross-call (crm/kanban) để agent tự gọi qua MCP endpoint sẵn có — không hard-code.
- **P11 — Moderation (OAuth)** ✅ code: `oauth.rs` — OAuth 2.0 Installed-App/loopback (Desktop client, scope `youtube.force-ssl`): user dán client_id/secret → `/api/oauth/start` mở consent → `/api/oauth/callback` đổi token (access+refresh, tự refresh khi hết hạn); secret+token chỉ nằm trong DB local. `comments.setModerationStatus` (held/published/rejected + banAuthor) qua `moderate()`; `moderation_url` guard **banWithoutReject** (ban chỉ với rejected). Tools `youtube_oauth_status`/`youtube_moderate` + REST `/api/oauth/*` + `/api/moderate`. Verify: 3 unit test (auth_url/moderation_url-rules/token-store) + runtime (authUrl Google chuẩn, guard, callback page). **Gọi mạng thật cần client OAuth của user.**

**Trạng thái: P6–P11 code-complete, 27 test pass, 20 MCP tool, repack youtube-app.zip 3.0M verified.** Chặn còn lại: verify với phiên đăng nhập thật (InnerTube) + credential OAuth thật (moderation Data API).

---

## 7. Rủi ro

- **Moderation cần đúng quyền**: `setModerationStatus`/`banAuthor` chỉ chủ kênh; `banAuthor` bắt buộc đi kèm `rejected` (nếu không → 400 `banWithoutReject`).
- **ToS**: tự động remove/report hàng loạt dễ bị coi là lạm dụng — giữ approve + throttle.
- **Sentiment LLM ≠ sự thật**: nhãn cảm xúc là gợi ý, không quyết định thay người; hiển thị kèm độ tin.
- **Quyền riêng tư**: comment public nhưng gắn vào khách hàng CRM là dữ liệu cá nhân — chỉ lưu cái cần, tôn trọng scope space.

## 8. Nguồn + file tham chiếu

- `comments.setModerationStatus` (held/reject/ban, owner-auth): developers.google.com/youtube/v3/docs/comments/setModerationStatus
- Analytics API metrics (không per-comment sentiment): developers.google.com/youtube/analytics/metrics
- InnerTube comment actions / creatorHeart: LuanRT/YouTube.js, haxzie/innerTube.js
- Điểm nối repo: CRM pull `apps/crm/src/channels/social.rs` + `db_inbox.rs`; feed `apps/social/src/api.rs` (`/inbox`,`/inbox/reply`); dashboard `apps/crm/src/db_dashboard.rs` + `api_dashboard.rs` + `mcp_ext.rs`; knowledge `app-space-sdk/src/bridge.rs` (`knowledge_*`); app-to-app `apps/search/src/transport/app_mcp.rs`; scheduler `src/gateway/ui_server/background.rs`.
