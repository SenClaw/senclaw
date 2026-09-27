# AI Discuss Team — Thiết kế app phòng thảo luận AI đa thành viên

> Space App `apps/discuss` — "phòng họp" nơi một đội AI (các member có bộ nhớ riêng)
> thảo luận một chủ đề do **BOSS (người dùng)** đặt ra, có **Thư ký AI** ghi biên bản,
> **Manager AI** điều phối độc lập (không tham gia nội dung), kho tài liệu chung,
> view 3D isometric kiểu AI Office, và kết quả cuối được phân loại theo mức độ
> chứng minh (thực tiễn / lý thuyết).

## 1. Yêu cầu gốc từ BOSS

1. BOSS là người dùng — đặt đề bài + tiêu chí kết quả, tham gia chat bất kỳ lúc nào, chốt kết quả.
2. **Thư ký AI** ghi nhớ/ghi chú nội dung thảo luận (biên bản, ý chính, quyết định).
3. **Member AI**: nhiều thành viên, mỗi member có **bộ nhớ riêng** (nhớ cả *thinking* đã dùng),
   được dùng **toàn bộ tool app MCP đang có trên hệ thống**.
4. Member **hoạt động song song** khi được yêu cầu.
5. **Manager**: theo dõi + điều phối, **không thảo luận nội dung**, nhưng **bắt member phát biểu**
   nếu member đó không tham gia. Manager **độc lập** — tự đánh giá khi nào thảo luận
   "đã đủ như BOSS yêu cầu" thì đề nghị chốt.
6. **Kho tài liệu chung** — mọi thành viên trong team truy cập được.
7. **3 loại luận điểm (phân luận)**:
   - `evidence` — *tìm kiếm có dẫn chứng*: phải kèm trích dẫn (tài liệu / kết quả search / URL).
   - `inference` — *suy diễn từ thông tin đã có*: nêu rõ suy ra từ luận điểm/tài liệu nào.
   - `creative` — *sáng tạo từ thông tin có thể chưa có*: ý tưởng mới, giả thuyết.
   - Kết luận cuối phải gắn mức chứng minh: **thực tiễn** (practical — có dẫn chứng kiểm được)
     hay **lý thuyết** (theoretical — suy diễn/giả thuyết, chưa kiểm chứng).
8. **Tốc độ bàn luận** đủ chậm để human (BOSS) theo kịp và chen vào (pacing cấu hình được).
9. **View 3D** isometric như AI Office (phòng họp, avatar, bong bóng thoại, trạng thái).
10. **Chat view** — đọc và tham gia trực tiếp.
11. Khi một member đưa ý kiến, member khác **phải xem xét phản hồi**: *đồng tình* (xét xem cần
    bổ sung gì) hoặc *phản đối* (**bắt buộc kèm dẫn chứng**), vận dụng công cụ tư duy
    (**6 chiếc mũ**) và các tool (Search, Zeach, News...).
12. Member phải **hiểu cách vận dụng tool**: Search = tìm nhanh liên hợp, Zeach = nghiên cứu sâu
    có trích dẫn, News = tin tức/dòng sự kiện, Thinking = khung 6 mũ/5W...

## 2. Vai trò

| Vai trò | Ai | Nhiệm vụ | Có phát biểu nội dung? |
|---|---|---|---|
| **BOSS** | Human | Đặt đề bài + tiêu chí; chen chat bất cứ lúc nào; duyệt/чối kết quả; đổi pace; ép chốt | Có (ưu tiên cao nhất — member phải phản hồi tin BOSS trước) |
| **Manager** | AI | Theo dõi participation, nhắc/bắt member im lặng phát biểu; đánh giá tiến độ so với tiêu chí BOSS (điểm 0–100 + checklist thiếu gì); đề nghị chốt khi đủ; **độc lập, không bàn nội dung** | Không (chỉ ghi chú điều phối `manager_note`) |
| **Thư ký** | AI | Sau mỗi vòng: cập nhật biên bản (ý chính, luận điểm mới, đồng thuận/bất đồng, quyết định, việc cần làm); soạn bản nháp **kết quả cuối** khi Manager chốt | Không (chỉ đăng `minutes`) |
| **Member** | AI ×N | Phát biểu luận điểm (gắn loại + mức chứng minh + trích dẫn), phản hồi ý kiến member khác (đồng tình/phản đối có dẫn chứng), gọi tool khi cần, tự ghi bộ nhớ + thinking | Có |

## 3. Vòng thảo luận (round engine)

```
BOSS tạo phiên: topic + yêu cầu kết quả (success criteria) + chọn members
                + pace (giây/lượt) + chế độ (tuần tự | song song)
  └─> Round r = 1,2,3...
       ├─ [song song?] tất cả member sinh lượt cùng lúc (join_all)
       │   [tuần tự]  từng member một, delay = pace giữa các lượt
       ├─ Lượt member: đọc tin mới kể từ lượt trước
       │    1. BẮT BUỘC phản hồi tin nhắn BOSS chưa trả lời (nếu có)
       │    2. BẮT BUỘC phản hồi các luận điểm đang mở của member khác:
       │         agree (+bổ sung nếu cần) | disagree (+dẫn chứng bắt buộc)
       │    3. Có thể nêu luận điểm mới (claim_type + provability + citations)
       │    4. Được gọi tool (agent.run với allowed_tools của member)
       │    5. Trả về JSON: reactions[], claims[], memory_notes[], thinking_summary
       ├─ Thư ký: cập nhật biên bản vòng r
       └─ Manager: chấm participation + tiến độ
            ├─ member im lặng ≥ K vòng → post manager_note "yêu cầu @X phát biểu"
            │   và vòng sau lượt của X bị "ép" (force=true, prompt nêu rõ lệnh Manager)
            ├─ score ≥ ngưỡng & checklist tiêu chí phủ đủ → đề nghị chốt
            └─ BOSS approve (hoặc ép chốt / từ chối kèm feedback → mở lại)
  └─> Chốt: Thư ký tổng hợp KẾT QUẢ — mỗi kết luận gắn:
        loại luận điểm | mức chứng minh (thực tiễn/lý thuyết) | dẫn chứng | ý kiến bảo lưu
```

- **Pacing**: `pace_seconds` (0 = nhanh nhất, mặc định 20s/lượt) — engine `sleep` giữa các lượt
  để BOSS đọc kịp; đổi được giữa chừng; có Pause/Resume. Tin BOSS gửi vào là "ngắt ưu tiên":
  member kế tiếp phải xử lý trước khi làm việc khác.
- **Song song**: chế độ `parallel` chạy các lượt member đồng thời (mỗi member một phiên
  agent.run cô lập), kết quả post theo thứ tự hoàn thành; pace áp giữa các vòng thay vì giữa các lượt.
- **Chống bế tắc**: `max_rounds` (mặc định 12) — chạm trần thì Manager buộc tổng kết với
  trạng thái "chưa đạt đủ tiêu chí", liệt kê phần thiếu.

## 4. Luận điểm, phản hồi & 6 mũ

- Mỗi phát biểu là `message`, trong đó **claim** (luận điểm) có cấu trúc:
  `{claim_type: evidence|inference|creative, provability: practical|theoretical,`
  `citations: [{kind: doc|url|tool, ref, quote}], hat: white|red|black|yellow|green|blue?}`
- **Phản hồi (reaction)**: `reply_to = message_id`, `stance: agree|disagree`,
  - `disagree` **bắt buộc** ≥1 citation (không có → engine trả lượt lại cho member kèm nhắc luật).
  - `agree` khuyến khích `supplement` (bổ sung gì cho luận điểm).
- Luận điểm "mở" = chưa được ≥1 member khác phản hồi → Manager theo dõi, member sau phải xử lý.
- **6 mũ**: prompt member hướng dẫn chọn mũ phù hợp từng phát biểu (white=dữ kiện, black=rủi ro,
  yellow=lợi ích, green=sáng tạo, red=trực giác — nêu ngắn, blue=quy trình, dành cho Manager).
  Không ép cứng mỗi vòng một mũ; mũ là nhãn metadata hiển thị trên UI (màu bong bóng thoại).

## 5. Bộ nhớ riêng của member

- Bảng `member_memory`: fact/stance/lesson member tự ghi sau mỗi lượt (`memory_notes[]`),
  kèm `discussion_id` nguồn; recall khi build prompt lượt sau (FTS theo topic + gần đây).
- Bảng `member_thinking`: `thinking_summary` (member tự thuật mạch suy nghĩ đã dùng) mỗi lượt —
  đáp ứng "nhớ cả thinking đã dùng"; lượt sau được nhét lại phần thinking gần nhất của chính
  member đó để giữ nhất quán lập trường.
- Bộ nhớ là **riêng tư per-member** (member khác không đọc được), tồn tại **xuyên phiên**.

## 6. Kho tài liệu chung

- Bảng `documents` + FTS5 (fold đ→d như apps/ba): BOSS upload (txt/md/pdf…), member đóng góp
  (kết quả tool đáng giữ được Thư ký lưu thành tài liệu), biên bản + kết quả các phiên cũ.
- Mọi vai trò đọc được; trích dẫn `doc:<id>#đoạn`. Tool `discuss_docs_search` cho cả UI lẫn member.

## 7. Tool cho member

- Member chạy bằng **agent.run** (bridge): mặc định `tools = null` → **toàn bộ tool MCP hệ thống**
  (đúng yêu cầu BOSS). BOSS có thể giới hạn per-member trong UI (soft-enforce, xem §9.0).
- System prompt member kèm "cẩm nang dùng tool" (đúng tên full identifier đã xác minh):
  - `mcp__search-mcp__search_query` / `search_ask` — tìm liên hợp nhanh, có nguồn + đếm nguồn độc lập;
  - `mcp__zeach-mcp__zeach_research` (depth quick|standard|deep) — nghiên cứu sâu → báo cáo trích dẫn `[n]`;
  - `mcp__news-mcp__news_latest` / `news_trends` / `news_search` — thời sự, xu hướng, dòng sự kiện;
  - `mcp__thinking-mcp__think_*` — dàn khung 6 mũ/5W khi cần cấu trúc hoá;
  - `mcp__senclaw-memory__memory_search`, `mcp__senclaw-wiki__wiki_search` — tri thức nội bộ;
  - `Read`/`Grep` trên workspace = kho tài liệu phiên.
- Member `use_tools=false` chạy `llm.request` thuần (nhanh, rẻ) — hợp vai suy diễn/sáng tạo.
- Danh sách tool khả dụng lấy động từ daemon `GET /api/mcp-servers` (ghép `mcp__{server}__{tool}`)
  → UI cho BOSS tick chọn per-member.

## 8. Kết quả & nghiệm thu

- `results`: bản tổng hợp cuối (markdown có cấu trúc) gồm: Kết luận chính (mỗi ý gắn
  loại + mức chứng minh + dẫn chứng), Bất đồng còn lại, Việc đề xuất, Nguồn.
- Luồng nghiệm thu: Manager đề nghị chốt → Thư ký soạn nháp → BOSS `approve` / `reject(feedback)`
  (reject → phiên mở lại, feedback thành tin BOSS ưu tiên). BOSS có thể `force_conclude` bất kỳ lúc nào.
- Manager **độc lập**: đánh giá bằng phiên LLM riêng, không nhìn nháp của Thư ký, chấm theo
  checklist tiêu chí BOSS (điểm + thiếu gì) — code quyết định ngưỡng, AI chỉ chấm từng mục.

## 9. Kiến trúc kỹ thuật

- **App**: `apps/discuss`, port **4760** (4750 đã thuộc widget-pack), MCP server **`discuss-mcp`**,
  tool prefix `discuss_*`. Khung theo thế hệ apps/ba + apps/study (package = id, config.rs đọc
  `SENCLAW_BIND_HOST` loopback-default, data dir `~/.senclaw/space-app-data/discuss` NGOÀI thư mục
  cài, schema.sql idempotent, FTS5 tự fold đ→d, vite `base: '/'`, pack.sh → `release/` phẳng,
  register-local trỏ `apps/discuss/release`).

### 9.0 Sự thật bridge đã kiểm chứng (2026-08-03)

- `agent.run` payload: `{prompt, system, space, workspace, timeoutSeconds (clamp 10–1800), tools[], model}`
  → response `{status, text, durationMs, usage{inputTokens,outputTokens}}`. **One-shot** — không có
  session nối tiếp, app tự nhét lịch sử vào prompt mỗi lượt; không huỷ được run đang chạy.
- **Trần song song cứng = 4 run đồng thời/app** (persona `space-app-discuss`, vượt → lỗi ngay
  "reached max concurrency", không xếp hàng) ⇒ engine dùng Semaphore(3) + retry/backoff.
- **`tools` allowlist hiện KHÔNG được daemon enforce** (`ZenVirtualCoreApi::execute_virtual_prompt`
  bỏ qua `_tools`) — mặc định mọi run thấy TOÀN BỘ tool pool (trừ Task/AskUserQuestion). Trùng khớp
  yêu cầu "member dùng toàn bộ tool MCP hệ thống"; hạn chế per-member vì vậy là **soft** (ghi trong
  system prompt + vẫn truyền payload `tools` để tự cứng khi daemon vá — đã tạo task vá riêng).
- `model` trong payload agent.run hiện bị bỏ qua (chạy model active toàn cục); `llm.request` có
  `profile` hoạt động thật. `mcp.call` chưa bật.
- Mọi agent.run dùng chung 1 tab browser (`agent_id "virtual-worker"`) ⇒ cẩm nang member khuyên
  dùng search/zeach/news thay vì browser; chế độ song song chấp nhận rủi ro này.
- `space` per member = `discuss:<member_key>` → bộ nhớ dài hạn daemon tách riêng từng member,
  cộng thêm bảng memory/thinking trong DB app (recall có kiểm soát + hiển thị UI).
- `workspace` per phiên = `<data_dir>/docs/<discussion_id>/` — tài liệu kho chung được ghi thành
  file .md tại đó nên member đọc trực tiếp bằng Read/Grep.
- **Backend** (Rust + axum, theo skeleton apps/ba):
  - `db.rs` — SQLite `~/.senclaw/space-apps/discuss/discuss.db`: `discussions`, `members`
    (roster + persona + allowed_tools + avatar/seat), `messages`, `reactions` (nếu tách),
    `documents`(+FTS), `member_memory`(+FTS), `member_thinking`, `minutes`, `results`,
    `participation` (last_round per member).
  - `engine.rs` — vòng round như §3 (tokio task per phiên active; pace sleep; parallel join_all;
    manager/secretary hooks; force-turn; đánh giá + chốt).
  - `llm.rs` — bridge: `agent.run` cho lượt member (tools), `llm.request` cho Thư ký/Manager
    (không cần tool). Parse JSON có guard (markup-guard như cognify).
  - `api.rs` — REST + WS/SSE cho UI (feed tin nhắn live, trạng thái member, pace control,
    boss chat, docs CRUD/upload, results, nghiệm thu).
  - `mcp.rs` — `discuss-mcp`: `discuss_create`, `discuss_start/pause/resume`, `discuss_say`
    (BOSS nói), `discuss_status`, `discuss_members_*`, `discuss_docs_add/search`,
    `discuss_minutes`, `discuss_result`, `discuss_conclude/approve/reject`…
- **Web UI** (React + Vite như ai-office): 2 chế độ xem đồng bộ cùng dữ liệu:
  1. **Chat view** — feed thảo luận (bubble theo vai trò, nhãn loại luận điểm + mũ màu,
     citations bấm được, ô nhập của BOSS, nút pace/pause/chốt).
  2. **Phòng họp 3D isometric** — bàn họp, avatar ngồi quanh, bubble ai đang nói,
     badge trạng thái (đang nghĩ/đang tìm kiếm/chờ), Manager đứng bảng theo dõi checklist,
     Thư ký góc bàn với sổ biên bản (render kỹ thuật giống ai-office).
  - Panel phải: Biên bản (live) | Kho tài liệu | Kết quả | Tiến độ tiêu chí của Manager.

## 10. Rủi ro & đối sách

| Rủi ro | Đối sách |
|---|---|
| Member "chém" dẫn chứng ảo | Luật: citation phải là doc id có thật / URL tool trả về; engine validate doc id; UI badge "nguồn không kiểm được" |
| agent.run song song đè nhau | Mỗi lượt một session cô lập (SENCLAW_AGENT_ID per member) + semaphore giới hạn đồng thời |
| JSON member trả sai format | Parser khoan dung (trích khối ```json), retry 1 lần kèm lỗi, quá thì lượt thành "im lặng" và Manager nhắc |
| Phiên chạy vô hạn | max_rounds + trần token/phiên + Pause mặc định khi BOSS offline lâu (tùy chọn) |
| Bridge maxTokens trần | Chia nhỏ prompt lượt (chỉ tin mới + recall chọn lọc), maxTokens theo bài học predict (≥1500) |

## 11. Lộ trình

1. ✅ Phân tích + thiết kế (tài liệu này)
2. Scaffold app + DB + manifest + đăng ký workspace
3. Engine round + Thư ký + Manager + memory/thinking
4. MCP tools + REST/WS
5. Web UI chat + isometric + docs + result
6. Tests + build + zip + register-local + verify
