# Study Space App — biến tài liệu thành lộ trình học có lịch, có kiểm tra, có dẫn chứng

> Trạng thái: **ĐÃ BUILD & VERIFY 2026-08-01.** 126 test xanh, zip 4.1 MB
> (`apps/study/study-app.zip`). Vá core `space_events.link` đã vào, 4 test riêng.
> Toàn bộ P0–P7 dưới đây đã hoàn thành; phần "Lộ trình build" giữ lại làm nhật ký.
>
> **Ba lỗi thật do test/kiểm thử phát hiện, đã sửa:**
> 1. `fold()` chỉ đổi `đ→d` mà không bỏ dấu → chấm cloze và dò thẻ trùng sai với
>    người gõ không dấu. Nay bỏ dấu đầy đủ.
> 2. Chunk gán về mục theo `char_start` → phần chồng lấn thò ngược sang mục
>    trước, khiến trích dẫn mang **tên chương sai**. Nay khoá theo điểm giữa.
> 3. `Form.useWatch` gọi trong callback `find` → React #310, **trắng cả tab**
>    Lộ trình. Nay hook chạy vô điều kiện ở đầu component.
>
> Một điểm nữa: `/api/today` từng trả phần việc **không kèm nội dung**, và phép
> cắt "phần i/n" bị viết hai lần (REST + MCP). Nay gộp vào `src/lesson.rs`.

- **App id**: `study` · **cổng**: `4720` (đã đối chiếu toàn bộ `apps/*/senclaw-manifest.json`, 4720 còn trống)
- **MCP**: server `study-mcp`, tiền tố tool `study_` → `mcp__study-mcp__study_*`
- **Runtime**: Rust + axum + rusqlite **0.32** (bundled), UI React/Vite trong `apps/study/web`

---

## 1. Vấn đề

Người học có tài liệu (PDF giáo trình, docx nội bộ, ghi chú markdown) nhưng
không có **lộ trình**. Việc tự chia "học bao lâu, hôm nay học gì, khi nào ôn"
là phần khó nhất và cũng là phần máy làm tốt hơn người. Yêu cầu gốc:

1. Upload tài liệu
2. AI/LLM bóc tách nội dung → chỉ mục → tổng hợp
3. Tạo chương trình học theo mong muốn (bao lâu / bao nhiêu ngày), có template gợi ý
4. Tự đẩy lịch vào calendar SenClaw; mở event → mở app tới đúng bài hôm nay
5. Trắc nghiệm sinh từ tài liệu để kiểm tra kiến thức
6. Tìm kiếm trong tài liệu, tổng hợp **có dẫn chứng** trỏ về tài liệu
7. Tìm kiếm mở rộng qua **MCP ngoài**, tổng hợp cũng phải có dẫn chứng
8. Chế độ học **flashcard** và **đọc bằng audio**

---

## 2. Prior art trong repo — cái gì port, cái gì gọi

Nguyên tắc: Space App là binary độc lập, **không share crate** với app khác →
port bằng cách **copy có chủ đích**, ghi rõ nguồn ở đầu file.

| Cần | Đã có sẵn ở | Cách dùng |
|---|---|---|
| Bóc text pdf/docx/html/csv | `apps/search/src/corpus.rs:47` (`pdf-extract 0.7` + `zip 2`) | **Port**. Giữ nguyên triết lý "PDF scan thì báo lỗi thật, không lưu rỗng" |
| Chunk theo đoạn + overlap, escape FTS5 | cùng file trên | **Port** |
| Thang SRS 7 cấp | `apps/kaen/src/srs.rs:18` — `INTERVALS_MIN = [30m, 1d, 3d, 7d, 30d, 90d]`, sai → 30 phút, snap "khung giờ vàng" theo timezone | **Port**, đổi đơn vị từ *từ vựng* sang *thẻ + khái niệm* |
| Trích dẫn `[n]` không bịa được | `apps/zeach/src/synthesize.rs:41` `number_evidence()` + `apps/zeach/src/claims.rs` ("model bịa id → citation không kiểm được") | **Port nguyên guard**: claim nào trỏ evidence id không tồn tại trong run thì loại |
| RRF + chọn nguồn đa dạng | `apps/search/src/fusion.rs` (`select_diverse`) | **Port** |
| Biến MCP bất kỳ thành nguồn tìm kiếm | `apps/search/src/sources/mcp_source.rs` (`McpSourceSpec`, `FieldMap`, `url_template`) | **Port** làm lớp mapping |
| Tự **phát hiện** MCP tra cứu, không hardcode URL | `apps/predict/src/evidence.rs` — `discover()` hỏi daemon `GET /api/mcp-servers`, `score_tool()` chấm điểm, `query_param()` dò tên tham số từ `inputSchema`, `extract_items()` nhận nhiều shape | **Port**. Đây là bản tiến hoá hơn preset tĩnh của `search` — dùng bản này |
| Sinh trắc nghiệm + chấm + giải thích | `apps/kaen/src/grammar.rs` | Tham khảo prompt/luồng, viết lại cho đơn vị *section tài liệu* |
| Đọc thành tiếng | daemon `POST /api/tts/synthesize` — `src/gateway/ui_server/tts.rs:508` | **Gọi thẳng**, không tự nhúng TTS |
| Gọi LLM | bridge `POST /api/space/apps/study/bridge` action `llm.request` | **Gọi**, xem mục 9 về giới hạn |

**Quan hệ với Kaen**: Kaen (`apps/kaen`, 4500) là *từ vựng ngoại ngữ* — đơn vị là
một từ (`word|nghĩa|ví dụ|IPA`). Study là *tài liệu bất kỳ* — đơn vị là một
section/khái niệm. Không gộp, không phụ thuộc; hai app cùng dùng một thang SRS
là chuyện tốt chứ không phải trùng lặp.

---

## 3. Luồng tổng thể

```
 tệp  ──► corpus.rs ──► outline.rs ──► index.rs ──► syllabus.rs
(pdf/docx/md)  bóc text    cắt mục       FTS5        tổng hợp +
               + note      + LLM tóm     + chunk     bản đồ khái niệm
                            mỗi mục       offset
                                              │
                                              ▼
                              planner.rs ◄── template (5 mẫu dựng sẵn)
                              + mong muốn user (số ngày, phút/ngày, deadline)
                                              │
                     ┌────────────────────────┼───────────────────────┐
                     ▼                        ▼                       ▼
              calendar.rs              study modes              ask.rs
        POST /api/space/calendar   đọc · flashcard(SRS)   FTS5 + MCP ngoài
        /events  (mỗi buổi 1 event) audio(TTS) · quiz     → RRF → LLM → [n]
                     │                   · recall            có dẫn chứng
                     ▼                        │
        event.link = /space/app/study?session=<id>
        bấm event / bấm nhắc  ──────────────► mở đúng buổi hôm nay
```

---

## 4. Bóc tách & chỉ mục

**4.1 Bóc text** — port `corpus.rs`. Hỗ trợ `txt md csv tsv json html htm pdf docx`.
Ba luật giữ nguyên từ bản gốc:
- PDF scan không có text layer → **báo lỗi nêu tên nguyên nhân**, gợi ý chạy qua
  `mcp__senclaw-ocr__ocr_*` rồi upload lại. Tuyệt đối không lưu doc rỗng.
- Đuôi lạ → từ chối theo tên, không lưu mojibake.
- Truy vấn người dùng **không phải** biểu thức FTS5 → phải escape trước khi ghép.

**4.2 Cắt mục (`outline.rs`)** — hai tầng, tầng máy chạy trước:
- *Xác định*: heading markdown `#`, đánh số (`Chương 3`, `1.2.4`), dòng IN HOA
  ngắn, dòng có số trang/mục lục. Không có heading → cắt theo đoạn với overlap.
- *LLM*: mỗi mục → `{title, summary, key_points[], concepts[], difficulty 1-5,
  est_minutes, prerequisites[]}`. Đây là input duy nhất mà planner cần từ LLM;
  mọi phép tính lịch sau đó là **số học thuần**, không hỏi model.

**4.3 Chỉ mục** — FTS5 trên `chunks`, mỗi chunk giữ `doc_id, section_id, ord,
char_start, char_end, page` để `[n]` nhảy về **đúng đoạn** chứ không chỉ đúng file.
Tokenizer `unicode61 remove_diacritics 2`, thêm bước fold `đ→d` ở tầng ứng dụng
(bài học `tiktok-dl`: FTS5 mặc định không fold `đ`, "dong" không ra "đông").

**4.4 Tổng hợp** — tóm tắt toàn tài liệu + **bản đồ khái niệm**
(`concepts` × `sections` many-to-many). Bản đồ này là xương sống cho: thứ tự học
(prerequisite), chọn câu hỏi thích ứng, và biết "khái niệm nào đang yếu".

---

## 5. Lập chương trình học

**Input**: mục tiêu, tổng số ngày *hoặc* deadline, phút/ngày, thứ trong tuần,
khung giờ ưa thích, trình độ hiện tại, template.

**Thuật toán (deterministic — không để LLM chia lịch):**
1. `budget = số buổi × phút/buổi`
2. Chuẩn hoá `est_minutes` các mục sao cho **Σ ≤ 70 % budget**; 30 % còn lại dành
   cho ôn + quiz. Đây là chỗ hiệu ứng giãn cách sống hay chết.
3. Sắp thứ tự tôpô theo `prerequisites`, rồi **xen kẽ** chủ đề (interleaving) —
   không dồn cả chương vào một buổi liên tiếp.
4. Rải mốc ôn sau lần học đầu ở `+1d, +3d, +7d, +16d` (kẹp trong tầm kế hoạch),
   cùng họ với thang SRS của Kaen.
5. Mỗi buổi **kết thúc bằng một khối truy hồi** (quiz ngắn hoặc recall) — testing effect.
6. **Không đủ thời gian thì nói thẳng**: "với nhịp này cần 47 ngày, bạn đặt 20" →
   đưa 3 lựa chọn (giãn ngày / tăng phút/ngày / cắt phạm vi, có liệt kê mục nào bị cắt).
   Không được lặng lẽ cắt bớt — cùng nguyên tắc "no silent caps" của repo.

**Template dựng sẵn** (bảng `plan_templates`, seed lúc boot đầu, user sửa được):

| Mẫu | Nhịp | Ôn | Dùng khi |
|---|---|---|---|
| Nước rút | 7 ngày × 60–90′ | 1d/2d/4d, quiz mỗi buổi | thi gấp |
| Chuẩn *(mặc định)* | 30 ngày × 30′ | 1/3/7/16 | học đều |
| Chuyên sâu | 60–90 ngày × 45′ | 1/3/7/16/35 + project + Feynman | học nghề |
| Vi mô | 3 phiên × 6′/ngày | dày, thẻ là chính | lịch bận |
| Ôn lại | 10 ngày × 20′ | chỉ quiz + flashcard, bỏ đọc | đã học rồi |

**Re-plan khi lỡ buổi**: event qua giờ mà buổi chưa `completed` → planner dồn lại
phần còn lại và **hiện diff cho user duyệt** (buổi nào đẩy sang ngày nào), không
tự sửa lịch trong im lặng.

---

## 6. Lịch SenClaw — chỗ duy nhất phải vá core

**Cái đã có**: bảng `space_events` (`src/db/schema.rs:608`), REST
`/api/space/calendar/events` (`src/gateway/ui_server/core.rs:562`, không có lớp
auth chặn app gọi), `EventNotifier` đã tự bắn `space:event:reminder` qua WS +
thông báo hệ thống (`src/scheduler/event_notifier.rs`), desktop đã có
`ReminderInteractionOverlay` (`desktop_app/lib/features/chat/reminder_interaction.dart`).

**Cái thiếu — đã kiểm chứng**: `space_events` **không có trường link/app nào**.
Cột hiện có: `title, description, start_at, end_at, all_day, location, color,
recurrence, reminder_min, task_id, source, status, …`. Nên hôm nay một sự kiện
**không thể trỏ về một màn hình app**. Đây chính xác là yêu cầu số 4.

**Đề xuất (nhỏ, tổng quát, mọi app hưởng lợi — không riêng Study):**

1. `src/db/schema.rs:534` — thêm vào đúng vòng lặp thêm cột đang có:
   `("link", "TEXT")` và `("app_id", "TEXT")`.
2. Cho `link` đi qua REST create/update và MCP `space_event_create` /
   `space_event_update` (`src/mcp/space_server.rs:367`).
3. Hiện nút chính **"Mở bài học"** khi `link` khác rỗng, ở 3 nơi:
   - web: `EventDetailDrawer` — `web/src/components/space/calendar/CalendarView.tsx:124`
   - desktop: `_DayEventsDialog` — `desktop_app/lib/features/space/space_screen.dart:1017`
   - hộp thoại nhắc: `reminder_interaction.dart` (đưa `link` vào `ReminderTarget`)
4. Giá trị link Study ghi vào: `/space/app/study?session=<id>`.
   **Không cần thêm gì nữa**: `SpaceAppFrame` đã forward nguyên query string của
   trang ngoài vào iframe (`web/src/components/space/SpaceAppFrame.tsx:22`), nên
   app mở thẳng đúng buổi.

*Chỉ chấp nhận `link` là đường dẫn nội bộ bắt đầu bằng `/space/app/` — không nhận
URL ngoài, để một app cài từ hub không biến sự kiện lịch thành bẫy click.*

**Phương án không vá core** (nếu muốn tránh đụng core): nhét link vào `description`.
Vẫn phải sửa UI để nó bấm được → không rẻ hơn, mà lại chỉ Study dùng được. **Khuyến nghị vá.**

---

## 7. Bốn chế độ học

**7.1 Đọc** — text mục + tóm tắt AI cạnh nhau; bôi đen → tạo thẻ / hỏi AI về đoạn đó.

**7.2 Flashcard + SRS**
- Nguồn thẻ: (a) LLM sinh từ mục — dạng Q/A, định nghĩa, và **cloze** (khoét chỗ
  trống ngay trong câu gốc, ít bịa nhất); (b) người học bôi đen; (c) khái niệm
  trong bản đồ.
- Thang: port `srs.rs` của Kaen — level 0..6, `[30m, 1d, 3d, 7d, 30d, 90d]`, sai →
  lịch lại sau 30 phút, level ≥ 2 snap vào khung giờ học của user **theo đúng
  timezone** (Kaen đã sửa lỗi kaizen đặt giờ trên `moment.utc()`; đừng lặp lại).
- Tự chấm 4 mức Quên/Khó/Được/Dễ → ánh xạ lên thang.
- Mỗi thẻ giữ `section_id` + `chunk_id` → luôn mở được ngữ cảnh gốc.

**7.3 Audio (TTS)**
- Gọi daemon `POST /api/tts/synthesize` — body `{text, voice?, language?, speed?, model_id?}`;
  thiếu thì rơi về setting đã lưu, cuối cùng là `macos-speech`.
- **Model chưa cài → API trả 400** (`src/gateway/ui_server/tts.rs:524`). App phải
  hiện đúng lỗi đó ("chưa cài giọng đọc — vào Cài đặt → TTS"), không nuốt lặng.
- Ba chế độ:
  - *Podcast*: đọc cả mục, cắt câu rồi phát liên tiếp (giống voice-chat đọc theo câu).
  - *Rảnh tay*: đọc mặt trước → chờ n giây → đọc mặt sau → tự sang thẻ. Học lúc đi bộ/lái xe.
  - *Đọc theo yêu cầu*: nút loa trên từng đoạn/thẻ.
- Cache WAV theo `hash(text, voice, speed)` trong data dir; TTS local đủ chậm để
  cache là bắt buộc chứ không phải tối ưu.

**7.4 Recall / Feynman** — người học gõ lại bằng lời mình; LLM chấm theo rubric so
với mục gốc, chỉ ra chỗ thiếu/sai, **kèm trích dẫn đoạn tài liệu**.

---

## 8. Trắc nghiệm sinh từ tài liệu

**Dạng câu**: 1 đáp án · nhiều đáp án · đúng/sai · điền khuyết (cloze) · nối cặp ·
sắp thứ tự.

**Chốt chống bịa (quan trọng nhất)**: mỗi câu hỏi **bắt buộc** mang
`evidence: {chunk_id, quote}`. Sau khi LLM trả về:
1. `chunk_id` không tồn tại trong run → **loại câu**.
2. `quote` không phải chuỗi con của chunk đó (sau chuẩn hoá khoảng trắng) → **loại câu**.

Đây đúng là guard của Zeach (`claims.rs`: model bịa id thì citation không kiểm được
mà lại trông như đã được chứng thực) — áp cho câu hỏi thay vì cho khẳng định.

**Chấm bằng code, không bằng model.** So đáp án là phép so; chỉ phần *giải thích*
mới nhờ LLM, và giải thích phải dựng trên `quote` đã xác minh.

**Vòng khép kín**: sai câu nào → tự sinh thẻ cho khái niệm đó, đẩy vào SRS ở
level 0. Ngân hàng câu hỏi giữ lại kèm thống kê; lần sau chọn **thích ứng** —
ưu tiên khái niệm có tỉ lệ sai cao và đến hạn ôn.

---

## 9. Hỏi đáp trong tài liệu, có dẫn chứng

`study_ask(question, scope)`:
1. **Truy hồi**: FTS5/BM25 trên `chunks` (+ tuỳ chọn `senclaw-cognitive`, `senclaw-memory`).
2. **Hợp nhất**: RRF rồi `select_diverse` — tránh lấy 5 chunk cùng một trang.
3. **Tổng hợp**: LLM chỉ được dùng chunk đã cấp; mỗi nhận định gắn `[n]` với `n`
   là **vị trí 1-based trong đúng danh sách evidence trả về cho caller** — đánh số
   do code làm (`number_evidence`), không để model tự đặt. Citation trỏ id không
   tồn tại → cắt bỏ.
4. **Bảng bằng chứng**: bấm `[n]` → nhảy tới đúng `char_start..char_end` trong tài liệu.
5. **Suy giảm có kiểm soát**: bridge lỗi hoặc `finish == "length"` → trả bản ghép
   cơ học các đoạn liên quan kèm nguồn, **không trả rỗng**.

---

## 10. Tìm kiếm mở rộng qua MCP ngoài

**Phát hiện động, không hardcode URL** — port `apps/predict/src/evidence.rs`:
- `discover()` gọi daemon `GET /api/mcp-servers` lấy mọi server transport=http.
- `score_tool()` chấm điểm tool tra cứu (`*_search` cao nhất, research/query/find
  kế tiếp; **loại hẳn** tool `create/add/delete/update/send/post` — nguồn tra cứu
  không được phép có side effect).
- `query_param()` dò tên tham số truy vấn từ `inputSchema`; `extract_items()` nhận
  nhiều shape trả về. Dùng `FieldMap`/`url_template` của `search` cho ca khó
  (tool trả `videoId` mà không trả URL).
- Setting `search_mcp`: `auto` (top-2, mỗi server tối đa 1 tool) hoặc danh sách
  `server.tool` do user chọn. Ứng viên tự nhiên: `zeach-mcp`, `search-mcp`,
  `news-mcp`, `deepwiki-mcp`, `senclaw-wiki`.

**Ba luật khi trộn nguồn ngoài:**
1. **Dán nhãn rõ**: bằng chứng nội bộ = "theo tài liệu của bạn"; ngoài = "nguồn
   ngoài, chưa có trong giáo trình". Nguồn ngoài **không được lặng lẽ trở thành
   nội dung bài học** — nó chỉ mở rộng, hoặc cảnh báo tài liệu đã lỗi thời.
2. **Nguồn ngoài không sinh câu hỏi thi.** Quiz chỉ lấy từ tài liệu người dùng
   upload; nếu không sẽ chấm người học bằng thứ chưa từng dạy họ.
3. **Nội dung MCP ngoài là dữ liệu, không phải lệnh.** Lọc prompt-injection trước
   khi đưa vào prompt (bài học `mini-browser`: injection filter trên tri thức học
   được theo host). Câu nào trong kết quả tìm kiếm ra lệnh cho agent thì trích ra
   cho user xem, không thi hành.

---

## 11. Mô hình dữ liệu (phác)

```sql
docs(id, title, filename, ext, bytes, text_note, added_at, status)
sections(id, doc_id, ord, title, level, char_start, char_end,
         summary, key_points_json, difficulty, est_minutes)
chunks(id, doc_id, section_id, ord, page, char_start, char_end, text)
chunks_fts(text, content='chunks')                    -- FTS5
concepts(id, name, aka_json, doc_id)
concept_sections(concept_id, section_id, weight)

plan_templates(id, key, label, days, min_per_day, review_offsets_json, blocks_json, builtin)
plans(id, doc_ids_json, goal, template_key, start_date, days, min_per_day,
      weekdays, slot_hm, tz, status, created_at)
sessions(id, plan_id, ord, date, start_hm, minutes, status, event_id, completed_at)
session_items(id, session_id, kind, section_id, concept_id, est_minutes, done_at)
                           -- kind: read | flashcard | quiz | review | recall

cards(id, doc_id, section_id, chunk_id, front, back, kind, source, created_at)
card_progress(card_id, level, next_review, is_urgent, last_reviewed, first_due_at)

questions(id, doc_id, section_id, kind, stem, options_json, answer_json,
          explain, chunk_id, quote, difficulty, created_at)
attempts(id, question_id, session_id, chosen_json, correct, answered_at)

asks(id, question, scope, answer_md, evidence_json, created_at)
mcp_sources(key, label, target_json, tool, query_arg, field_map_json, weight, enabled)
tts_cache(hash, voice, speed, path, bytes, created_at)
settings(k, v)
```

---

## 12. MCP tools (`study-mcp`, ~26 tool)

| Nhóm | Tool |
|---|---|
| Tài liệu | `study_doc_upload` (text thuần), `study_doc_list`, `study_doc_outline`, `study_doc_summary`, `study_doc_delete` |
| Chỉ mục | `study_reindex`, `study_concepts` |
| Kế hoạch | `study_templates`, `study_plan_create`, `study_plan_preview` (xem trước, chưa ghi lịch), `study_plan_list`, `study_plan_replan`, `study_plan_delete` |
| Lịch | `study_calendar_sync` (đẩy buổi → `space_events`), `study_today` (buổi hôm nay + link mở) |
| Học | `study_session_open`, `study_session_complete`, `study_cards_due`, `study_card_review`, `study_cards_generate` |
| Kiểm tra | `study_quiz_generate`, `study_quiz_take`, `study_quiz_grade`, `study_weak_concepts` |
| Tra cứu | `study_ask` (nội bộ, có `[n]`), `study_research` (nội bộ + MCP ngoài), `study_sources` (liệt kê nguồn MCP phát hiện được) |
| Đọc | `study_speak` (trả URL audio đã cache) |

Skill kèm theo: `study-coach` (triggers: "lên lịch học", "học tài liệu này",
"hôm nay học gì", "tạo đề trắc nghiệm", "ôn bài", "tra trong tài liệu"…) +
persona `study-coach`.

---

## 13. Lộ trình build

| Pha | Nội dung | Xong khi |
|---|---|---|
| P0 | corpus + outline + FTS5 + UI upload/đọc | upload PDF thật → thấy mục lục + tóm tắt |
| P1 | planner + 5 template + preview | xem trước lịch 30 ngày, không ghi gì |
| P2 | **vá core `link`** + `calendar_sync` + deep-link | bấm event → mở đúng buổi |
| P3 | flashcard + SRS + TTS | học rảnh tay bằng audio |
| P4 | quiz + guard evidence + chấm bằng code | đề sinh ra, câu bịa bị loại |
| P5 | `study_ask` + bảng bằng chứng | `[n]` nhảy đúng đoạn |
| P6 | MCP ngoài (`discover` + nhãn nguồn) | `study_research` trả nguồn có nhãn |
| P7 | tiến độ, streak, đường cong quên, báo cáo | — |

P2 là pha duy nhất đụng vào core; nên tách commit riêng.

---

## 14. Bẫy đã biết (đừng dẫm lại)

1. **Bridge `llm.request` chỉ nhận `system/prompt/maxTokens/profile`** — không có
   `temperature`, không stream. `finish == "length"` **là lỗi**, không phải kết quả
   ngắn. → cắt chunk nhỏ, `maxTokens ≤ 32000`.
   [[space-app-llm-bridge-no-temperature]] · [[space-app-llm-bridge-output-ceiling]]
2. **Data dir phải nằm NGOÀI thư mục cài** — cài lại zip là `remove_dir_all(app_dir)`,
   DB để cạnh binary sẽ bay mỗi lần update. Theo đúng `apps/kaen/src/config.rs:29`.
3. `rusqlite` phải **0.32** (lệch version là lỗi link — bài học `hub`).
4. Vite `base: '/'`, nếu không deep-route `?session=` hỏng (bài học `kaen`).
5. Bind `SENCLAW_BIND_HOST`, **không** `0.0.0.0` (bài học tự phơi API).
6. Cắt preview bằng `&s[..N]` sẽ panic với tiếng Việt → `truncate_on_char_boundary`.
7. FTS5: truy vấn user không phải biểu thức FTS5 (`giá "vàng" - SJC` là syntax error
   *và* là injection vector) → escape. Thêm fold `đ→d`.
8. Đừng `lsof -ti tcp:PORT | xargs kill` khi debug — giết cả socket client, chết daemon.
   Dùng `-sTCP:LISTEN` hoặc `pgrep` theo tên binary.
9. Space App có thể còn tiến trình mồ côi giữ cwd cũ → 404/UI cũ sau khi cài lại;
   `lsof` + kill + respawn.
10. Link ngoài trong UI phải mở **trình duyệt hệ thống**, không điều hướng webview —
    theo `docs/space-app-open-external.md`.

---

## 15. Câu hỏi cần chốt trước khi code

1. **Vá core `link` cho `space_events`** — đồng ý không? Không có nó thì yêu cầu
   "mở event → mở bài học" không thực hiện được đúng nghĩa.
2. Tên hiển thị của app (id `study` cố định): "Study", "Lộ Trình Học", hay tên khác?
3. Pha P0–P2 (upload → lịch chạy được) trước, rồi mới flashcard/quiz — hay muốn
   flashcard sớm hơn?
