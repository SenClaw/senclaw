# Siêu Dự Đoán Space App — dự báo AI đa lĩnh vực có kiểm chứng

> **Nâng cấp 2026-07-30 (9) — nguồn tìm kiếm = MCP ĐỘNG, bỏ URL cứng.** Yêu cầu
> user: "chọn mcp hoặc động mcp để search tìm kiếm thông tin, không set địa chỉ".
> `evidence.rs` viết lại: `discover()` hỏi daemon `GET /api/mcp-servers` (lấy
> server transport=http có `url`), `score_tool()` chấm điểm công cụ tra cứu
> (`*_search` 100 · research 75 · query 45 · find 40; +news/web/research; −code/
> graph/json/test; loại hẳn tool create/add/delete/update/send/post), `select()`
> chọn **auto top-2** (mỗi server tối đa 1 tool) hoặc đúng danh sách `server.tool`
> người dùng chọn. Gọi tool tổng quát: `query_param()` tự dò tên tham số truy vấn
> từ `inputSchema` (query/q/keyword/text/question/term + limit/count/max_results),
> `extract_items()` nhận nhiều shape trả về (`evidence`/`results`/`items`/
> `articles`/`hits`/`docs`/`posts`/`matches`, mảng trần, hoặc text thuần → 1 tài
> liệu); mỗi bằng chứng gắn `source = "<server>.<tool>"`. Setting `search_mcp`
> (`auto` hoặc CSV keys) thay `search_app_url`; REST `GET /api/search-sources`.
> UI Cài đặt: Segmented **Tự động / Chọn thủ công** + multi-select + bảng nguồn
> kèm điểm. 69 tests. Verified sống: quét ra **26 nguồn**, auto chọn
> `zeach-mcp.zeach_search` + `news-mcp.news_search`, `predict_ask` lấy 15 bằng
> chứng có nhãn nguồn.

> **Nâng cấp 2026-07-30 (8) — kho TÀI LIỆU ngoài số liệu.** Yêu cầu user: "ngoài
> trường động còn có tài liệu thông tin ngoài số liệu theo ngày, theo giá trị".
> Bảng `topic_docs (title, content, date, ref)` — `date` gắn tài liệu với một
> ngày (khớp bản ghi cùng ngày), `ref` gắn với một trường/giá trị. REST
> `GET/POST /api/topics/:key/docs` + `DELETE /:did`; 2 MCP tools mới
> (`predict_topic_doc_add`, `predict_topic_docs`) → **26 tools**. Tài liệu vào
> `analyze`/`derive_rules` (15 mới nhất) và vào `ask` qua `relevant_docs()`
> (ưu tiên khớp từ khoá dài của câu hỏi, còn chỗ thì lấp bằng mới nhất — tài liệu
> có ngày xếp trước); prompt cả ba nơi được dặn coi `documents` là **bằng chứng
> ngang hàng số liệu** và dùng để giải thích bất thường. UI: card "Tài liệu &
> thông tin ngoài số liệu" trong dashboard (thêm/tìm/xoá, chọn `ref` từ danh sách
> trường) + badge số tài liệu ở grid Tổng quan. 65 tests. Verified sống: lưu tin
> "sương giá Minas Gerais 26/07" vào chủ đề cà phê → `predict_ask` trả p=0.82,
> trích đúng tin đó trong evidence_for và cộng adjustment +0.15.

> **Nâng cấp 2026-07-30 (7) — chủ đề tách TĨNH / ĐỘNG.** Yêu cầu user: "vị trí,
> thành phố, tài liệu hướng dẫn phân tích, giải thích, prompt là **tĩnh**; ngày
> tháng, giờ, nhiệt độ, gió là **động** theo dữ liệu đầu vào". `topics` thêm cột
> `static_json` (map bối cảnh cố định) + `guide` (tài liệu hướng dẫn phân tích =
> prompt riêng của chủ đề) qua migration; `topic::parse_static` nhận cả object và
> mảng `[{name,value}]` từ UI. `topic_meta_ctx()` gắn `static`+`guide` vào MỌI
> lời gọi AI của chủ đề (analyze / derive rules / ask), sf_synthesize được dặn
> tuân thủ `guide` và dùng `static_context`. `design_topic` giờ trả `static`,
> `fields`, `guide`, `sample_questions` — AI tự tách hai loại. Connector tự ghi
> `vị trí` / `giải` vào static khi tạo và khi đổi nguồn. UI builder + modal Sửa
> chia 3 khối rõ ràng (TĨNH / hướng dẫn / ĐỘNG); dashboard hiện thẻ "Bối cảnh cố
> định & hướng dẫn phân tích". 64 tests. Verified sống: wish "trồng rau Đà Lạt…
> dự đoán sương muối" → tĩnh {Địa điểm: Đà Lạt, Thời điểm đo: Buổi sáng, Đối
> tượng canh tác: Rau}, động {Ngày, Nhiệt độ, Độ ẩm, Sức gió, Có sương muối},
> guide nêu đúng ngưỡng <4-5°C + lặng gió và cạm bẫy bỏ qua yếu tố gió.

> **Nâng cấp 2026-07-30 (6) — nguồn dữ liệu thuộc về CHỦ ĐỀ, settings chung gọn
> lại.** Phản hồi user: "các setting này không cần thiết, sẽ phải config trong
> chủ đề không phải setting chung". `db::cities()/leagues()` không còn đọc setting
> toàn cục mà **suy ra từ `source_json` của các chủ đề connector** (leagues fallback
> EPL để MCP football tools của agent vẫn có dữ liệu). Thêm `POST /api/topics/:key/source`
> (`topic_source_update_value`) đổi địa điểm (geocode tự do) / giải (id + tên hiển
> thị), fetch ngay rồi sync; nếu tên chủ đề vẫn ở dạng mặc định ("Thời tiết X" /
> "Bóng đá Y") thì **tự đổi tên theo nguồn mới** và chuyển domain sổ điểm, còn tên
> do user đặt thì giữ nguyên. Builder + card "Nguồn dữ liệu của chủ đề này" nhập
> tự do (chip gợi ý chỉ là lối tắt). `/api/settings` rút còn `search_app_url`,
> `theme`, `active_sources` (read-only) + gợi ý; bỏ `/api/leagues`. Verified sống:
> đổi weather Hà Nội→Đà Lạt→Nha Trang, đổi football 4328→4335 tự rename "Bóng đá
> La Liga" + nạp 1 trận, `active_sources` phản ánh đúng. 61 tests.

> **Nâng cấp 2026-07-30 (5) — settings động, theme sáng/tối, tri thức sửa được.**
> (a) **Cài đặt động**: bỏ danh sách cứng — `POST /api/places {query}` geocode
> qua Open-Meteo Geocoding (keyless) lưu vào setting `custom_places`,
> `POST /api/leagues {id,name}` → `custom_leagues`; `db::city_coord` /
> `league_label` hợp nhất built-in + custom, `weather_value` và
> `engine::ensure_weather` dùng toạ độ tuỳ chỉnh nên thêm được nơi bất kỳ
> (verified: Buôn Ma Thuột 23–29.6°C, mưa 99%). (b) **Theme**: Segmented
> Sáng/Tối/Theo hệ thống ở header, lưu localStorage + đồng bộ setting `theme`,
> theo dõi `prefers-color-scheme` khi ở chế độ system; body background & color-scheme
> đồng bộ để không lộ viền. (c) **Tri thức sửa được**: `methodology` chuyển từ
> hằng số thành DB-override — `default_methodology()` là seed, `normalize()` lọc
> mục rỗng và **tự lấp phần thiếu bằng mặc định** (tri thức không bao giờ rỗng),
> `methodology_json(db)` trả bản đang dùng + cờ `customized`; `GET/POST /api/method`
> (+`{reset:true}`), `GET /api/method/default`, MCP `predict_method {update|reset}`;
> checklist người dùng sửa được bơm thẳng vào `sf_synthesize`. UI tab Tri thức có
> chế độ sửa inline cho source/checklist/pipeline/nguyên tắc/kỹ thuật. 60 tests.

> **Nâng cấp 2026-07-28 (4) — build chủ đề TỰ DO + sửa chủ đề.** Theo phản hồi
> "template chỉ là có sẵn, cho phép người dùng tự tạo theo mong muốn, không gò
> bó" + "hỗ trợ sửa chủ đề": modal build có 2 chế độ — **✨ Tự do (mặc định)**:
> người dùng mô tả bằng lời thường → `llm::design_topic` (`POST
> /api/topics/design`) trả **proposal** (tên, mô tả, trường, câu hỏi dự đoán mẫu)
> **sửa được trực tiếp** trước khi tạo, và luôn có lối tự điền trường bỏ qua AI;
> **Template** chỉ còn là lối tắt cho 4 connector. MCP: `predict_topic_create
> {wish}`. **Sửa chủ đề**: nút Sửa trên dashboard → `POST /api/topics/:key`
> (`db::update_topic`) đổi tên/mô tả/thêm-bớt-đổi kiểu trường; đổi tên gọi
> `rename_prediction_domain` nên sổ điểm & track record không đứt gãy. Verified
> sống: wish "bán bánh mì sáng…" → AI thiết kế 5 trường đúng kiểu + 3 câu hỏi
> mẫu; đổi tên "Giá cafe" → "Cà phê Robusta" chuyển đúng 4 dự đoán sang
> `topic:cà-phê-robusta`. 59 unit tests.

> **Nâng cấp 2026-07-28 (3) — topic-centric: bỏ tab mặc định, công cụ build chủ
> đề + dashboard riêng.** Theo yêu cầu user (kèm screenshot 4 tab cũ): UI chỉ còn
> 5 tab — Tổng quan (grid chủ đề + nút **Build chủ đề**) / Chủ đề (dashboard
> riêng) / Sổ dự đoán / Tri thức / Cài đặt. `builder.rs` cung cấp 5 template:
> gold, weather{city}, lottery, football{league}, blank; tạo từ template gắn
> `topics.source_json` và `engine::sync_topic` tự nạp bản ghi từ dữ liệu local
> đã fetch (dedup theo ngày/event_id, chạy trong run_all mỗi 10' + nút Sync +
> POST /api/topics/:key/sync). Dashboard riêng `GET /api/topics/:key/dashboard`:
> thẻ thống kê per-field, chart chuỗi thời gian (series_by_date), dữ liệu, quy
> luật & bài học, sổ điểm + dự đoán mở của riêng domain chủ đề, ô hỏi siêu dự
> báo. `predict_topic_create` nhận `template`+`params` (giữ 24 tools). Các engine
> mặc định (Elo, XSMB, thời tiết, vàng) **vẫn chạy ngầm** phục vụ MCP tools cho
> agent — chỉ gỡ khỏi UI. Verified sống: build 4 connector topics một phát ăn
> ngay (XSMB +15 kỳ, vàng/thời tiết/bóng đá +1), dashboard + chart render, 57
> unit tests xanh, zip 3.2M.

> **Nâng cấp 2026-07-28 (2) — pipeline Siêu Dự Báo theo sách Superforecasting
> (Tetlock).** Bốn tầng theo yêu cầu: (a) **Nền tảng tri thức đánh giá** —
> `methodology.rs` mã hoá 11 điều răn + kỹ thuật của sách, hiển thị ở tab
> **Tri thức** + tool `predict_method`, và bơm vào prompt tổng hợp như checklist
> bắt buộc; (b) **Nền tảng dữ liệu** — thống kê số học per-field
> (`topic::numeric_summary`), quy luật + bài học, track record per-domain
> (`db::track_record`); (c) **Tổng hợp thông tin ngoài** — `evidence.rs` gọi
> **Search app** app→app (JSON-RPC 4530, cấu hình `search_app_url`; app News
> 4640 đang được build có thể thành nguồn thứ hai sau); (d) **Pipeline tổng hợp**
> — `predict_ask` = Fermi decompose (LLM) → gather (data + news) → synthesize
> theo checklist Tetlock ra trace {outside_view/base_rate, evidence for/against,
> adjustments, premortem, p, confidence, update_triggers} (normalize + clamp ở
> `evidence::normalize_trace`), fallback về single-call khi fail (mode "simple").
> Khi resolve, tự chạy **postmortem** (`sf_lesson`) lưu bài học vào chủ đề
> (topic_rules source='lesson'). 24 MCP tools, 52 unit tests, zip 3.2M.
> Verified sống: decompose ra 4 câu hỏi con chuẩn + Search app trả 14 bằng chứng
> cho topic Giá cafe; bước synthesize chưa verify sống trọn vẹn vì **daemon tắt
> giữa chừng phiên test** (fallback + normalize đã có unit test) — cần chạy lại
> `POST /api/ask` khi daemon bật.

> **Refactor 2026-07-28 — "form chung" (theo yêu cầu bổ sung).** Thêm tầng
> **Chủ đề tùy chỉnh** tổng quát bên trên 4 lĩnh vực dựng sẵn: user tự định nghĩa
> chủ đề + schema trường (text/number/date/bool) → nhập tay / import CSV·JSON /
> tìm kiếm → AI **phân tích dữ liệu** → AI **rút quy luật siêu dự đoán** từ lịch
> sử (kèm độ tin cậy, lưu bảng `topic_rules`, quy luật user thêm tay giữ riêng)
> → hỏi **"điều X có xảy ra không?"** (`predict_ask`) trả `p_yes` + lý do kiểu
> superforecaster, tự ghi sổ domain `topic:<tên>` (mỗi chủ đề có Brier/accuracy
> riêng trong `predict_score`). Verified live với LLM thật (ag/gemini-pro-agent):
> chủ đề "Giá cafe" 7 bản ghi → AI rút đúng quy luật mưa→giá tăng đã gài trong
> dữ liệu, ask trả p=0.58 với lập luận base-rate chuẩn mực. MCP mở rộng lên **23
> tools** (+8 `predict_topic_*` / `predict_ask`), UI thêm tab **Chủ đề**, 47 unit
> tests. Gotcha mới ghi nhận: model reasoning trên bridge trả text RỖNG khi
> maxTokens nhỏ (ngân sách bị suy nghĩ ẩn ăn hết) → mọi lời gọi llm.request dùng
> budget ≥1500.

**Status:** **BUILT & verified live (P1–P5, 2026-07-27)** · **App:** `apps/predict` · **Port:** 4600 · **MCP:** `predict-mcp`
**Date:** 2026-07-27

> **Build status.** Toàn bộ backend + UI + MCP đã build và chạy thật: 38 unit
> tests xanh; boot fetch thật kéo về 583 CLB Elo, **7504 kỳ XSMB** (backfill, có
> kỳ hôm nay), vàng $4093.5 + USD/VND 26 257 → 129.589 triệu/lượng, thời tiết
> Hà Nội live; 15 MCP tools trả JSON-RPC chuẩn; UI 7 tab render tốt (dark mode);
> `predict-app.zip` 3.1MB. Khác thiết kế: λ Poisson suy từ hiệu Elo (chưa dùng
> attack/defense từ football-data — cần key); morning brief là tool/REST
> (`predict_brief`), chưa push draft qua channel; XSMN/XSMT & V-League & SJC nội
> địa chưa có (đúng như dự kiến phase sau).

> Yêu cầu gốc: *"app chuyên biệt cho siêu dự đoán"* — một Space App chuyên về dự đoán bằng
> AI + dữ liệu thật: **bóng đá, xổ số (thống kê), thời tiết, giá vàng/tỷ giá**. Điểm khác biệt
> so với mọi app "siêu dự đoán" trên chợ: **mọi dự đoán đều được ghi sổ và tự chấm điểm khi có
> kết quả thật** (Brier score / accuracy) — dự đoán có kiểm chứng, không chém gió.

---

## 1. Mục tiêu & phạm vi

Bốn domain dự đoán + một lớp kiểm chứng xuyên suốt:

| Domain | Nội dung | Cơ sở |
|---|---|---|
| ⚽ Bóng đá | Xác suất 1X2, tỷ số khả dĩ, Tài/Xỉu 2.5 cho trận sắp diễn ra; bài nhận định kiểu "siêu máy tính" | Elo + Poisson model trên dữ liệu thật |
| 🎰 Xổ số | Kết quả XSMB + thống kê (tần suất, lô gan, đầu-đuôi, chu kỳ); mục "chốt số" **giải trí có disclaimer** | Thống kê mô tả — trung thực rằng xổ số là ngẫu nhiên |
| 🌦 Thời tiết | Dự báo 7 ngày theo vùng + tóm tắt lời khuyên tiếng Việt | Open-Meteo (model best-match) |
| 🪙 Vàng & tỷ giá | Giá XAU thế giới, SJC nội địa, USD/VND; xu hướng SMA/momentum + bình luận AI | Lịch sử giá tự tích lũy |
| 📒 Sổ dự đoán | Ledger mọi dự đoán → auto-resolve khi có kết quả → Brier/accuracy per domain, biểu đồ calibration | USP của app |

**Non-goal:** không tích hợp cá cược/đặt lệnh dưới mọi hình thức; không đưa lời khuyên đầu tư
cá nhân hóa (mọi output vàng/tỷ giá kèm disclaimer "chỉ tham khảo, không phải lời khuyên đầu tư");
mục xổ số chỉ phân tích kết quả xổ số kiến thiết chính thống, không dính lô đề.

---

## 2. Nguồn dữ liệu (đã verify sống 2026-07-27)

| Nguồn | Endpoint | Key? | Trạng thái verify |
|---|---|---|---|
| **ClubElo** — Elo mọi CLB châu Âu | `http://api.clubelo.com/<YYYY-MM-DD>` (CSV) | Không | ✅ trả Elo hiện tại (Arsenal 2063.7, top 1) |
| **XSMB dataset** (khiemdoan/vietnam-lottery-xsmb-analysis) | `raw.githubusercontent.com/.../data/xsmb.csv` (+ JSON/Parquet, cả `xsmb-2-digits`) | Không | ✅ cập nhật daily qua GitHub Actions, **đã có kỳ quay hôm nay 2026-07-27**, đủ cơ cấu giải ĐB→G7 |
| **Open-Meteo** | `api.open-meteo.com/v1/forecast?latitude=…&daily=…` | Không | ✅ Hà Nội 3 ngày OK, có precipitation_probability |
| **gold-api.com** — XAU thế giới | `https://api.gold-api.com/price/XAU` | Không | ✅ $4078.60, cập nhật realtime |
| **open.er-api.com** — tỷ giá | `https://open.er-api.com/v6/latest/USD` | Không | ✅ đủ VND |
| **TheSportsDB** | `thesportsdb.com/api/v1/json/3/eventsnextleague.php?id=4328` | Key test `3` | ✅ fixtures EPL 2026-27, có map `idAPIfootball` |
| **ESPN keyless** | `site.api.espn.com/apis/site/v2/sports/soccer/eng.1/scoreboard` | Không | ✅ EPL OK; ❌ `vie.1` (V-League) trả 400 |
| **football-data.org** | REST, 12 giải lớn, 10 req/phút | Free key (đăng ký) | Chưa test (cần key) — nguồn fixtures/standings chính thống |
| **API-Football** | 1236 giải (có V-League) | Free 100 req/ngày | Chưa test — phương án V-League |
| **SJC XML** | `sjc.com.vn/xml/tygiavang.xml` | Không | ❌ chặn Cloudflare "Just a moment" khi gọi server-side |
| **BTMC API** | `api.btmc.vn/api/BTMCAPI/getpricebtmc?key=…` | Key public | ❌ không phản hồi từ mạng hiện tại |
| **vnappmob gold** | `api.vnappmob.com/api/v2/gold/sjc` | Free key (đăng ký) | Chưa test — phương án SJC chính |

**Kết luận nguồn:** P1 chạy được **hoàn toàn keyless** (ClubElo + XSMB CSV + Open-Meteo +
gold-api + er-api + TheSportsDB). Vàng SJC nội địa và V-League cần key đăng ký free hoặc
crawl qua `senclaw-browser` — đẩy sang phase sau. XSMN/XSMT chưa có nguồn keyless sạch
(ManyCai cần tài khoản; hoặc crawl xoso.com.vn) — phase sau, P1 chỉ XSMB.

---

## 3. Phương pháp dự đoán

### 3.1 Bóng đá — Elo + Poisson, LLM chỉ diễn giải

Pipeline mỗi trận:

1. **Elo probability**: lấy Elo hai đội từ ClubElo, cộng lợi thế sân nhà (~65 Elo);
   `P(home) = 1 / (1 + 10^(-(eloH + 65 - eloA)/400))`, tách hòa theo mô hình Davidson
   (hoặc tỉ lệ hòa lịch sử theo giải ~25%).
2. **Poisson tỷ số**: λ mỗi đội = attack_strength × defense_strength đối thủ × mean_goals
   của giải (tính từ kết quả mùa hiện tại, fetch football-data/TheSportsDB); ma trận
   P(i:j) cho i,j ∈ 0..6 → tỷ số khả dĩ nhất, P(Over 2.5), P(BTTS).
3. **Blend**: trung bình có trọng số Elo-prob và Poisson-prob (Elo 60/Poisson 40 khởi điểm,
   tune sau bằng chính sổ calibration).
4. **LLM synthesis** (bridge `llm.request`): viết bài nhận định tiếng Việt kiểu
   "siêu máy tính dự đoán" từ số liệu đã tính — prompt chốt cứng: *LLM không được bịa
   hay sửa xác suất, chỉ diễn giải*; đưa form 5 trận gần nhất + H2H làm màu bài viết.

Đầu ra chuẩn: `{p_home, p_draw, p_away, best_score, p_over25, p_btts, article_vi}` —
đồng thời ghi 1 dòng vào sổ dự đoán với `due_at = kickoff`.

### 3.2 Xổ số — thống kê trung thực, chốt số là giải trí

- Ingest toàn bộ lịch sử `xsmb.csv` (backfill 1 lần, ~vài nghìn kỳ) + fetch daily.
- Thống kê: tần suất loto 00–99 (7/30/90/365 ngày), **lô gan** (số ngày chưa về),
  đầu–đuôi, tần suất giải ĐB, chu kỳ về lại.
- "Chốt số giải trí": chọn theo heuristic thống kê (VD: top tần suất 30 ngày ∩ gan sắp
  đạt chu kỳ trung bình) + LLM viết lời bình — **mọi response bắt buộc kèm disclaimer**:
  *"Xổ số là ngẫu nhiên — không hệ thống nào dự đoán được kết quả. Nội dung chỉ mang tính
  thống kê & giải trí."* Sổ dự đoán vẫn ghi & chấm để chứng minh trung thực (hit-rate sẽ
  hội tụ về xác suất nền ~27% cho 1 cặp loto/kỳ 27 giải).

### 3.3 Thời tiết — relay + humanize

Open-Meteo daily/hourly cho danh sách thành phố cấu hình (mặc định Hà Nội, HCM, Đà Nẵng);
LLM tóm 1 đoạn lời khuyên ("mai mưa 99%, mang áo mưa; nồm ẩm nên chưa phơi đồ").
Không tự chế model — forecast là của Open-Meteo, app ghi sổ P(mưa) để chấm calibration
(so với `precipitation` observed hôm sau — Open-Meteo có API archive để lấy actuals).

### 3.4 Vàng & tỷ giá — xu hướng, không phải lời khuyên

- Tích lũy `price_history` (XAU, SJC nếu có nguồn, USD/VND) mỗi giờ.
- Chỉ báo: SMA7/SMA30, momentum 24h/7d, khoảng dao động; naive forecast ngày mai =
  hôm nay + drift trung bình (làm baseline, kèm khoảng tin cậy).
- LLM commentary xu hướng + **disclaimer đầu tư bắt buộc** trong UI và mọi MCP response.

### 3.5 Sổ dự đoán & calibration (USP)

```
predictions(id, domain, subject, detail_json, probs_json, predicted_at, due_at,
            resolved_at, outcome_json, brier, correct)
```

- Mọi dự đoán từ mọi domain đều insert vào đây (kể cả dự đoán user tự nhập qua MCP/UI —
  "generic prediction": *"Việt Nam thắng Thái Lan"*, P=0.7, hạn 2026-08-10).
- Scheduler auto-resolve: bóng đá đối chiếu kết quả trận (ESPN/TheSportsDB), xổ số đối
  chiếu kỳ quay, thời tiết đối chiếu archive Open-Meteo, generic thì hỏi user/notify.
- Chấm: Brier score `(p - outcome)²`, accuracy, và **biểu đồ calibration** (dự đoán 70%
  thì có đúng ~70% không) per domain — hiển thị công khai trên tab Sổ dự đoán.

---

## 4. Kiến trúc

Theo chuẩn Space App hiện hành (mẫu: luna-calendar/zeach/facebook-pro):

- **Backend**: Rust + axum + rusqlite (giữ 0.32 nếu workspace yêu cầu — xem gotcha hub),
  reqwest fetchers, tokio interval scheduler nội bộ.
- **UI**: React + Vite + AntD, 7 tab: **Tổng quan** (brief hôm nay: thời tiết + vàng +
  trận đáng chú ý + số liệu xổ số hôm qua) · **Bóng đá** · **Xổ số** · **Thời tiết** ·
  **Vàng & Tỷ giá** · **Sổ dự đoán** (ledger + calibration chart) · **Cài đặt**
  (API key football-data/API-Football/vnappmob, thành phố, giải/đội theo dõi).
- **LLM**: bridge `llm.request` — nhớ 2 gotcha đã ghi memory: *không có tham số
  temperature* và *output ceiling / finish=="length" phải coi là lỗi* → prompt ngắn,
  số liệu đã tính sẵn, không nhét bảng dài.
- **MCP**: JSON-RPC http tại `/api/mcp/sse` (manifest `mcp.autoRegister: true`),
  tên `predict-mcp`, prefix tool `predict_*`.
- **Manifest**: id `predict`, name "Siêu Dự Đoán", icon 🔮, port **4600** (4590 đã là
  facebook-pro), bridge caps `space.rest` + `llm.request`.

### 4.1 Scheduler nội bộ

| Giờ (GMT+7) | Việc |
|---|---|
| 06:00 | Fetch thời tiết + soạn **morning brief** (draft/notify qua channel — theo gate draft-first như moltbook/facebook-pro) |
| Mỗi giờ | Giá vàng/tỷ giá → `price_history` |
| 18:35 | Fetch kỳ XSMB mới + resolve dự đoán xổ số + cập nhật thống kê |
| Ngày có trận theo dõi | Poll kết quả mỗi 15' sau giờ kickoff → resolve dự đoán bóng đá |
| 03:00 Thứ 2 | Refresh Elo snapshot + attack/defense strengths |

### 4.2 DB schema (rút gọn)

```
teams(id, name, league, elo, atk_strength, def_strength, updated_at)
matches(id, league, home_id, away_id, kickoff, status, home_goals, away_goals, source_ref)
lottery_draws(date PK, special, prize1..prize7_4, loto_json)   -- import từ xsmb.csv
price_history(ts, asset, buy, sell, source)                    -- XAU/SJC/USDVND
weather_cache(city, date, payload_json, fetched_at)
predictions(...)                                               -- §3.5
settings(key, value)
```

---

## 5. MCP tools (`predict-mcp`, 15 tools)

| Tool | Mô tả |
|---|---|
| `predict_football_match` | Dự đoán 1 trận (đội nhà, đội khách): 1X2 %, tỷ số, O/U, bài nhận định |
| `predict_football_today` | Quét fixtures hôm nay/ngày mai của các giải theo dõi, dự đoán hàng loạt |
| `predict_football_table` | BXH + Elo hiện tại của giải |
| `predict_lottery_results` | Kết quả XSMB kỳ mới nhất / theo ngày |
| `predict_lottery_stats` | Tần suất, lô gan, đầu-đuôi theo cửa sổ ngày |
| `predict_lottery_suggest` | "Chốt số giải trí" + disclaimer bắt buộc |
| `predict_weather` | Dự báo 7 ngày theo thành phố + lời khuyên |
| `predict_gold_price` | Giá XAU/SJC/tỷ giá hiện tại |
| `predict_gold_trend` | Chỉ báo SMA/momentum + bình luận AI + disclaimer |
| `predict_make` | Ghi 1 dự đoán generic (subject, probability, due) vào sổ |
| `predict_list` | Liệt kê dự đoán (lọc domain/trạng thái) |
| `predict_resolve` | Resolve tay 1 dự đoán generic (outcome) |
| `predict_score` | Báo cáo calibration: Brier/accuracy per domain |
| `predict_brief` | Morning brief tổng hợp (thời tiết + vàng + bóng đá + xổ số hôm qua) |
| `predict_status` | Trạng thái nguồn dữ liệu, lần fetch cuối, key nào thiếu |

## 6. Skills · Personas · Widgets

- **Skill `sieu-du-doan`** — triggers: "dự đoán bóng đá hôm nay", "kèo tối nay",
  "dự đoán tỷ số", "siêu máy tính dự đoán", "thống kê xổ số", "lô gan", "chốt số",
  "kết quả xổ số", "mai có mưa không", "dự báo thời tiết", "giá vàng hôm nay",
  "vàng lên hay xuống", "tỷ giá đô", "dự đoán của tôi đúng bao nhiêu"…
- **Persona `sieu-du-doan-master`** — chuyên gia dự đoán điềm đạm: luôn nêu xác suất +
  cơ sở dữ liệu, luôn kèm disclaimer đúng domain, không bao giờ khẳng định chắc chắn.
- **Widgets**: `predict-brief` (small — thời tiết + vàng hôm nay), `predict-football-today`
  (medium — trận nổi bật + % dự đoán), refresh 30–60'.

## 7. Phases

| Phase | Nội dung | DoD |
|---|---|---|
| **P1 — Data core (keyless)** | Fetchers ClubElo/XSMB/Open-Meteo/gold-api/er-api/TheSportsDB + schema + backfill XSMB + REST + UI khung 7 tab | Fetch thật chạy, dữ liệu vào DB, UI xem được kết quả xổ số + thời tiết + giá vàng |
| **P2 — Football engine** | Elo+Poisson model, fixtures, dự đoán + bài nhận định LLM, auto-resolve kết quả | Dự đoán 1 vòng đấu EPL thật, sổ ghi & tự chấm sau vòng đấu |
| **P3 — Lottery stats** | Thống kê đầy đủ + chốt số giải trí + disclaimer, resolve daily 18:35 | Số liệu khớp đối chiếu tay 3 kỳ; disclaimer hiện ở mọi đường ra |
| **P4 — Ledger & calibration** | predict_make/list/score, calibration chart, morning brief + draft/notify | Brier tính đúng trên fixture test; brief gửi draft qua channel |
| **P5 — Đóng gói** | 15 MCP tools + skill + persona + widgets + manifest + zip <50MB, test đủ | Cài qua Space Apps, gọi tool từ chat được |

## 8. Rủi ro & gotchas

1. **Nguồn có key**: football-data.org / API-Football / vnappmob đều cần user tự đăng ký
   free key → tab Cài đặt phải có chỗ dán key + `predict_status` báo thiếu key rõ ràng.
   P1 chạy keyless được nhờ ClubElo + TheSportsDB.
2. **SJC nội địa**: cả 2 endpoint public đều chặn/chết từ server → nếu vnappmob không ổn,
   crawl qua `senclaw-browser` (đúng chuẩn, không thay bằng MCP browser khác) hoặc chấp
   nhận chỉ có XAU + quy đổi.
3. **V-League**: ESPN không có (`vie.1` 400) — chỉ có qua API-Football free 100 req/ngày;
   để user bật trong Cài đặt, không mặc định.
4. **Pháp lý/đạo đức**: xổ số = thống kê + giải trí, disclaimer cứng trong code (không
   phải trong prompt); vàng/tỷ giá = không lời khuyên đầu tư cá nhân hóa; bóng đá =
   không tích hợp odds nhà cái/đặt cược.
5. **LLM bridge**: không temperature, output ceiling — bài nhận định giữ ≤~500 từ,
   `finish=="length"` coi là lỗi và retry với prompt ngắn hơn (memory đã ghi).
6. **Bẫy quen thuộc**: static-dir + SPA fallback cho deep-route (vite `base '/'` như kaen);
   `&s[..N]` panic UTF-8 → dùng `truncate_on_char_boundary`; port 4600 chưa ai dùng;
   zip release <50MB.
7. **Rate limit**: football-data 10 req/phút → cache fixtures theo ngày, không gọi trong
   request path của UI; mọi fetcher đều đi qua scheduler + cache DB.

## 9. Nguồn tham khảo

- ClubElo API: http://clubelo.com/API — Elo CSV theo ngày, free
- XSMB dataset: https://github.com/khiemdoan/vietnam-lottery-xsmb-analysis — CSV/JSON/Parquet daily
- Open-Meteo: https://open-meteo.com/ — forecast + archive (chấm calibration), free non-commercial
- gold-api.com: https://gold-api.com — XAU realtime keyless
- ExchangeRate open API: https://open.er-api.com/v6/latest/USD
- TheSportsDB: https://www.thesportsdb.com/free_sports_api — key test `3`
- football-data.org: https://www.football-data.org/ — free tier 12 giải, 10 req/phút
- API-Football: https://www.api-football.com/ — free 100 req/ngày, có V-League
- Tổng hợp API VN (vàng/thời tiết/xổ số): https://raccoon.vn/chi-tiet-tin-tuc/cac-api-mien-phi-lay-thong-tin-gia-vang-thoi-tiet-ket-qua-xo-so-221
