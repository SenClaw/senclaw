# Picture Book Space App — Nghiên cứu (research)

> Trạng thái: **research / chưa implement**. Mục tiêu: một Space App biến **nội
> dung có sẵn** (truyện đã viết lại, transcript, báo cáo, văn bản dán vào) thành
> **truyện tranh minh hoạ** và **ebook minh hoạ**, xuất được **PDF in KDP**,
> **EPUB3 fixed-layout**, **flipbook web**, và bàn giao được sang `video-flow` /
> `social`.
>
> App sẽ nằm ở `senclaw-app-private/apps/picture-book` (port đề xuất **4480**),
> tài liệu này nằm ở repo daemon theo đúng quy ước của `shopee-app-research.md`
> và `youtube-app-research.md` — hai app cũng sống trong repo private.

## 1. Yêu cầu đã chốt

| Hạng mục | Chốt |
|---|---|
| Loại sách | **Truyện tranh minh hoạ** (comic mini, có panel + thoại) **và ebook minh hoạ** (chữ là chính, ảnh chèn) |
| Nguồn ảnh | **Ba đường song song**: (A) cầu Chrome extension → Google Flow/Imagen, (B) API model trả phí (Gemini / OpenAI / Fal / Replicate), (C) API model trỏ thẳng (endpoint người dùng tự khai) |
| Đầu ra | **PDF in được (KDP)**, **EPUB3 fixed-layout**, **flipbook web trong app**, **bàn giao sang video-flow / social** |

Bốn đầu ra đó không phải bốn nút export của cùng một tài liệu. PDF in và EPUB
FXL đòi **trang cố định theo pixel và inch**; flipbook đòi **ảnh nhẹ, xem nhanh**;
bàn giao video-flow đòi **markdown cảnh, không cần ảnh**. Kiến trúc phải giữ một
"trang" ở dạng **mô hình dữ liệu**, không phải ở dạng file — rồi render bốn lần.

## 2. Hai sản phẩm, một pipeline

```
nội dung thô
   │
   ├─ outline ──── beat ──── page/panel        ← LLM, chỗ hai sản phẩm rẽ nhau
   │
   ├─ cast (rút nhân vật) ─ model sheet ─┐
   ├─ style bible ──────────────────────┤     ← ảnh tham chiếu, sinh MỘT lần/sách
   │                                     ▼
   └─ prompt từng panel ──────────► sinh ảnh ──► asset 300dpi
                                                    │
                    ┌───────────────┬───────────────┼───────────────┐
                    ▼               ▼               ▼               ▼
                PDF (KDP)      EPUB3 FXL       flipbook       screenplay .md
```

| | Truyện tranh minh hoạ | Ebook minh hoạ |
|---|---|---|
| Đơn vị | **panel** (2–6 panel/trang, lưới) | **block** (đoạn chữ) + ảnh chèn ở mốc |
| Chữ | thoại trong bong bóng + caption | thân bài chảy tự do |
| Tỉ lệ ảnh/chữ | ~90/10 | ~25/75 |
| Đích tự nhiên | PDF in, flipbook | EPUB3, PDF |
| Số ảnh/sách 32 trang | 60–120 | 8–16 |

Khác nhau nằm gọn ở **ba stage**: `paginate`, `layout`, `export`. Phần đắt tiền
nhất — nhất quán nhân vật, sinh ảnh, đạt 300dpi — dùng chung. Nên **một app, một
cột `book_kind`**, không phải hai app.

## 3. Vị trí trong fleet

**Nội dung vào** — không app nào phải viết thêm gì:

| Nguồn | Đường |
|---|---|
| `rewrite-story` | `GET /api/stories/:id/export?format=screenplay` — đã có, đã trả `# Cảnh N` |
| `youtube` | transcript / phân tích video |
| `ai-office` | báo cáo tổng hợp của văn phòng AI |
| Người dùng | dán text, upload .txt/.md/.docx |

**Ràng buộc cứng phải thiết kế quanh nó: app không gọi được app.**

Bridge của daemon khai `["llm.request", "agent.run", "mcp.call", "space.rest"]`
nhưng thực tế:

- `llm.request` — chạy thật (`src/gateway/ui_server/space.rs:2260`)
- `agent.run` — chạy thật (`space.rs:2169`)
- `knowledge.save` / `knowledge.search` / `knowledge.recall` — chạy thật (`space.rs:2381`, `:2431`)
- `mcp.call` — trả `{"status":"pending","message":"mcp.call bridge action is not enabled yet."}` (`space.rs:2532`)
- `space.rest` — **không có match arm** → rơi vào `_ =>` trả `400 Unknown bridge action` (`space.rs:2538`)

Nên "bàn giao sang video-flow" **không** phải là gọi `vf_generate_image`. Nó là
**file + skill**, đúng cách `rewrite-story/src/export.rs` đã làm: xuất screenplay
markdown theo hợp đồng heading `# Cảnh N`, rồi để một agent giữ cả hai MCP server
kéo nó qua. Chép nguyên mô hình đó, đừng phát minh lại.

**Dùng chùa được từ daemon** (không tốn key riêng): `llm.request`, `agent.run`,
`knowledge.*`, và REST ngoài bridge — `POST /api/tts/synthesize`, `GET /api/tts/models`
(xem `apps/video-flow/src/tts.rs:9`). TTS mở đường cho **flipbook có giọng đọc** và
**EPUB3 Media Overlay** ở v2, miễn phí.

## 4. Bốn bài toán lõi

Đây là phần quyết định app này sống hay chết. Ba trong bốn cái đã có lời giải
chạy được trong fleet.

### 4.1 Nhất quán nhân vật xuyên suốt cuốn sách

`video-flow` đã giải bài này và giải đúng — chép, đừng nghĩ lại:

1. Sinh **model sheet** (turnaround) cho từng nhân vật: *"the SAME person shown in
   a … neutral background"* (`apps/video-flow/src/process.rs:108`), tỉ lệ 16:9 để
   xếp được nhiều góc cạnh nhau (`process.rs:121`).
2. Lưu `reference_image_url`, **mirror ngay về local** vì URL ký của Flow hết hạn
   (`process.rs:153`).
3. Khi sinh ảnh cảnh: đẩy model sheet vào `imageInputs` với
   `"imageInputType": "IMAGE_INPUT_TYPE_REFERENCE"` (`process.rs:847`).
4. **Đồng thời nhắc lại ngoại hình bằng CHỮ** trong prompt cảnh (`process.rs:216`,
   `:1884`). Ảnh tham chiếu và chữ giữ nhau — chỉ một trong hai thì trôi.

Sách ảnh **khó hơn video** ở đúng một điểm, và điểm đó rất đắt: người đọc nhìn
**ảnh tĩnh**, lật qua lật lại, và **hai trang nằm cạnh nhau trên cùng một spread**.
Lệch màu tóc ở giây thứ 9 của video không ai thấy; lệch màu tóc giữa trang 12 và
trang 13 thì đập vào mắt. Nên cộng thêm:

- **Style bible**: một ảnh mẫu phong cách sinh một lần cho cả sách, luôn nằm trong
  `imageInputs` cùng với nhân vật. Không có nó thì trang 3 màu nước, trang 20 3D.
- **Seed cố định theo sách**, không phải `timestamp % 1_000_000` như `process.rs:838`
  — video-flow cố tình muốn mỗi lần một khác, sách thì ngược lại.
- **Model sheet nhiều biểu cảm/tư thế**, không phải một ảnh chân dung.
- Nano Banana Pro nhận tới **14 ảnh tham chiếu** → đủ chỗ cho 2–3 nhân vật +
  bối cảnh + style bible cùng lúc. Đây là lý do kỹ thuật để ưu tiên nó, không
  phải vì nó "xịn hơn".

### 4.2 Chữ trong ảnh — quy tắc: KHÔNG BAO GIỜ sinh chữ vào ảnh

Bong bóng thoại, caption, tiêu đề trang: **render vector lúc dàn trang**, ảnh chỉ
là nền. Năm lý do, mỗi lý do tự nó đã đủ:

1. **Tiếng Việt có dấu.** Model sinh chữ latin không dấu thì tạm được; "ế", "ữ",
   "ỵ" thì sai, và sai một cách không sửa được.
2. **Sửa một chữ = sinh lại cả ảnh**, tức là mất luôn tính nhất quán vừa đánh đổi
   ở 4.1 để có.
3. **Không dịch được.** Sách bán ở hai thị trường thì phải sinh lại toàn bộ ảnh.
4. **Không tái dùng được cho EPUB reflow** và không đọc được bằng TTS/screen reader.
5. **Không đạt nét ở 300dpi.** Chữ raster phóng lên khổ in bị răng cưa; chữ vector
   thì sắc ở mọi dpi. KDP còn đòi **font nhúng** và **cỡ tối thiểu 7pt** — chữ
   nằm trong ảnh thì không có font nào để nhúng cả.

Hai hệ quả bắt buộc phải nằm trong thiết kế ngay từ đầu:

- Mọi prompt ảnh gắn hậu tố phủ định: `no text, no letters, no speech bubbles, no
  watermark, no signature`.
- Prompt phải **chừa chỗ trống** cho bong bóng: mô tả bố cục kiểu *"nhân vật lệch
  phải, một phần ba trên bên trái là trời trống"*. Bảng `panel` vì thế có cột
  `quiet_zone` (`top-left` / `top` / `bottom` …) đi vào cả prompt lẫn thuật toán
  đặt bong bóng. Không có nó thì bóng thoại luôn đè lên mặt nhân vật.

### 4.3 300 DPI — ràng buộc chọn provider số một

KDP: **ảnh tối thiểu 300 DPI**, khuyến nghị trần 600 DPI; file có bleed **bắt buộc
PDF**; **font nhúng hết**; **≤ 650MB**; **≥ 24 trang**; không crop mark, không
annotation, phẳng hết transparency. Bleed **0.125″ trên cả bốn cạnh**.

Kích thước ảnh thật sự cần, tính sẵn:

| Trim (in) | Full-bleed (in) | Pixel @300dpi | Tỉ lệ | Ghi chú |
|---|---|---|---|---|
| 8.5 × 8.5 | 8.75 × 8.75 | **2625 × 2625** | **1:1 chẵn** | khổ vuông thiếu nhi — **khuyến nghị** |
| 8.25 × 8.25 | 8.5 × 8.5 | 2550 × 2550 | 1:1 chẵn | khổ vuông nhỏ hơn |
| 6 × 9 | 6.25 × 9.25 | 1875 × 2775 | 0.676 | không khớp tỉ lệ model nào |
| 5.5 × 8.5 | 5.75 × 8.75 | 1725 × 2625 | 0.657 | " |
| 8.25 × 6 (ngang) | 8.5 × 6.25 | 2550 × 1875 | 1.36 | " |
| 6 × 9 tràn đôi trang | 12.25 × 9.25 | 3675 × 2775 | 1.32 | spread |

Hai điều rút ra, và cả hai đều là quyết định sản phẩm chứ không phải chi tiết kỹ thuật:

- **Khổ vuông 8.5×8.5 là khổ nên mặc định cho truyện tranh minh hoạ.** Nó vừa là
  khổ sách thiếu nhi chuẩn của KDP, vừa **khớp đúng 1:1 — tỉ lệ gốc của mọi model
  sinh ảnh**. Mọi khổ khác (2:3, 0.657…) không trùng tỉ lệ nào model sinh được,
  nên phải sinh dư rồi crop — mà crop thì chính là thứ bleed sinh ra để chịu, nên
  vẫn làm được, chỉ là tốn pixel và mất kiểm soát bố cục ở mép.
- **Cạnh dài cần ≥ 2625px.** Model trả 1024 hoặc 2048 **không đủ in**. Đây là tiêu
  chí loại provider, và nó xếp **trên** giá:

| Độ phân giải model trả | Đủ cho | Kết luận |
|---|---|---|
| 1024 | flipbook, EPUB | không in được |
| 2048 | EPUB, PDF khổ ≤ 6×9 nếu chấp nhận ~220dpi | dưới chuẩn KDP |
| 4K (4096) | mọi khổ ở bảng trên | **đạt** |

Ước lượng file: 40 trang × 2625² JPEG q85 ≈ 1.5–3 MB/trang → **60–120 MB**, an
toàn dưới trần 650MB.

### 4.4 Chia trang / chia panel

Không dùng chunk đều. Một trang sách là **một beat kể chuyện**, dài ngắn tuỳ chỗ.
Cách rẻ và ổn định nhất là hai tầng, đúng mô hình `parse_blocks` của video-flow:

1. **Cắt thô bằng code**: tái dùng `apps/rewrite-story/src/text.rs` — bộ chia hybrid
   đã hiểu tiếng Việt, bảo vệ tiêu đề Chương/Hồi, ngắt theo chuyển cảnh bằng
   TF/cosine. Rẻ, xác định, không tốn lượt LLM.
2. **Chia beat bằng LLM trên từng đoạn thô**: mỗi beat → một trang (ebook) hoặc một
   panel (comic), đồng thời **tách `dialogue[]` ra khỏi `narration`** ngay tại đây —
   vì 4.2 đòi thoại phải là dữ liệu riêng, không phải chữ trong ảnh.

Rồi một bước **ép khổ sách in** mà không ai nhớ cho tới lúc upload bị KDP từ chối:
tổng số trang phải **≥ 24** và sách in **luôn chẵn**. App phải có stage `paginate`
biết gộp beat mỏng, tách beat dày, và chèn trang trắng/trang đề tặng cho đủ bội số.
Cảnh báo sớm trong UI, đừng để lộ ra lúc export.

## 5. Lớp sinh ảnh — ba đường, một trait

```rust
#[async_trait]
pub trait ImageProvider {
    fn id(&self) -> &str;
    fn max_edge_px(&self) -> u32;          // để 4.3 loại provider tự động
    fn max_reference_images(&self) -> usize;
    fn supports_headless(&self) -> bool;
    async fn generate(&self, req: ImageReq) -> Result<ImageOut, String>;
}
```

| | A. Extension → Google Flow | B. API trả phí | C. Endpoint tự khai |
|---|---|---|---|
| Model | `GEM_PIX_2` (`process.rs:15`) | Gemini 2.5/3 Pro Image, OpenAI, Fal, Replicate | OpenAI-compatible, ComfyUI local |
| Xác thực | token `ya29.*` extension bắt từ `labs.google` | API key người dùng | tuỳ người dùng |
| Chi phí | ~0đ (theo tài khoản) | ~$0.039/ảnh (2.5 Flash Image) · ~$0.13 (3 Pro 1–2K) · ~$0.24 (3 Pro 4K) | tuỳ |
| Ảnh tham chiếu | có (`IMAGE_INPUT_TYPE_REFERENCE`) | có (Nano Banana Pro: tới 14) | tuỳ |
| Headless | **không** — cần Chrome mở + reCAPTCHA | có | có |
| Độ phân giải | **phải đo** (xem §8 spike) | 4K có sẵn ở 3 Pro | tuỳ |
| Rủi ro | URL ký hết hạn → phải mirror ngay; reCAPTCHA; đứt khi đổi UI Flow | tốn tiền thật; app giữ key | không kiểm soát chất lượng |

Chi phí một cuốn 32 trang comic (~80 ảnh, 4K): **~$19** ở Nano Banana Pro, **~$3**
ở 2.5 Flash Image (nhưng không in được). Con số này phải hiện trong UI **trước khi
bấm chạy**, không phải sau.

Chiến lược khuyến nghị: **B làm mặc định, A làm đường rẻ, và cho phép chuỗi fallback
theo từng sách.** Draft duyệt bố cục bằng 2.5 Flash Image ($3), duyệt xong mới
render bản in bằng 3 Pro 4K — vì bản nháp và bản in dùng **cùng prompt, cùng
reference, cùng seed**, nên nhìn gần như nhau. Đây là cách duy nhất khiến app này
không đốt tiền của người dùng vào những trang họ sẽ vứt.

### Vấn đề nguyên tắc: app không được giữ provider key

Cả fleet có đúng một luật — *"they never hold a provider API key of their own"*
(README repo private). Đường B phá luật đó. Hai lối ra:

- **Khuyến nghị: xin daemon thêm bridge action `image.request`**, đối xứng với
  `llm.request` đã có. Key nằm ở daemon, cả fleet dùng chung, `video-flow` cũng
  được hưởng (nó đang buộc phải mở Chrome chỉ để sinh một tấm ảnh tham chiếu).
  Đây là thay đổi ở `app-space-sdk` + `space.rs`, không lớn, và **đúng chỗ**.
- **Tạm thời nếu chưa có**: key lưu ở `~/.senclaw/space-app-data/picture-book/`
  chmod 600, khai host trong `permissions.network` của `senclaw-hub.json`, không
  bao giờ log, không gửi đi đâu ngoài host đã khai. Ghi rõ trong README rằng đây
  là ngoại lệ có ý thức, kèm ngày, để sau này không ai tưởng là chuẩn.

## 6. Kiến trúc đề xuất

`apps/picture-book` — Rust axum, **port 4480** (trống: fleet đang dùng 4390, 4420,
4440, 4460, 4470, 4491, 4492, 4520, 4580, 4590, 4670, 4760), MCP `picture-book-mcp`,
tools `pb_*`.

| File | Vai trò |
|---|---|
| `src/db.rs` | SQLite; schema §6.1 |
| `src/llm.rs` | cầu `llm.request` (chép từ `rewrite-story`/`video-flow`) |
| `src/text.rs` | **chép `rewrite-story/src/text.rs`** — chia hybrid tiếng Việt |
| `src/paginate.rs` | beat → page/panel; ép ≥24 trang & chẵn |
| `src/cast.rs` | rút nhân vật, style bible, model sheet |
| `src/imagegen/mod.rs` | `trait ImageProvider` + chuỗi fallback |
| `src/imagegen/flow.rs` | đường A — port `video-flow/src/process.rs` phần ảnh |
| `src/imagegen/api.rs` | đường B/C — HTTP provider |
| `src/extbridge.rs` | WS extension (chép `video-flow/src/extbridge.rs`) |
| `src/mediastore.rs` | mirror ảnh về local (chép `video-flow/src/mediastore.rs`) |
| `src/layout.rs` | mô hình trang: panel grid, bong bóng, quiet zone |
| `src/render/pdf.rs` | sinh `.typ` → PDF (§7) |
| `src/render/epub.rs` | EPUB3 FXL, zip tay |
| `src/render/handoff.rs` | screenplay `# Cảnh N` cho video-flow; ảnh lẻ cho social |
| `src/dag.rs` `src/pipeline.rs` | engine + kế hoạch (chép `video-flow`) |
| `src/api.rs` `src/mcp.rs` | REST + MCP |
| `souls/` | prompt từng stage, sửa được từ UI (mô hình `video-flow`/`ai-office`) |

### 6.1 Schema

```
book        id, title, kind(comic|ebook), trim_size, bleed, dpi, page_target,
            style_bible_media_id, seed, provider_chain, status
character   id, book_id, name, appearance, model_sheet_media_id
page        id, book_id, index, kind(cover|content|blank), layout_template
panel       id, page_id, index, narration, dialogue_json, prompt,
            quiet_zone, image_media_id, image_status
asset       id, path, w, h, mime, original_url          -- mirror như video-flow
job         id, book_id, type, status, cost_estimate, error
export      id, book_id, format(pdf|epub|flipbook|screenplay), path, created_at
```

`asset.w/h` không phải trang trí: `render/pdf.rs` phải **từ chối** export in khi
có asset dưới ngưỡng 300dpi của khổ đã chọn, và nói rõ trang nào — thay vì giao
một PDF mà KDP sẽ từ chối sau 20 phút upload.

### 6.2 Pipeline DAG

`ingest → outline → paginate → cast → style-bible → model-sheets → panel-prompts
→ images → proof → export`

`proof` là stage rẻ và đáng giá nhất: kiểm bằng code (không tốn LLM) — đủ dpi
chưa, đủ trang chưa, chẵn chưa, panel nào chưa có ảnh, bóng thoại có đè quiet zone
không, chữ có dưới 7pt không.

## 7. Dàn trang & xuất bản

### PDF in — khuyến nghị `typst-as-lib`

Hai lựa chọn thật sự:

| | `typst-as-lib` | `printpdf` |
|---|---|---|
| Ngắt dòng, shaping tiếng Việt | có sẵn | **tự viết** |
| Bong bóng thoại co theo chữ | viết bằng `.typ` | tự tính hộp |
| Nhúng + subset font | có | có |
| Kích thước binary | **29.3 MB → zip 13 MB** (đo ở §8.1) | nhẹ |

Sách này đầy chữ tiếng Việt có dấu, trong hộp thoại co giãn theo nội dung. Chọn
`printpdf` nghĩa là **tự viết engine typography** — line breaking, kerning, dấu
chồng. Đó không phải việc của app này. Sinh `.typ` rồi để Typst lo là đúng phân
vai.

**Đã đo — xem §8.1.** Kết luận ngắn: Typst **đạt về chất lượng, vừa khít về dung
lượng**. Zip dự phóng ~15–17MB so với trần 20MB của hub. Chọn được, nhưng từ đây
trở đi **dung lượng zip là ràng buộc chặt nhất của app**, không phải tốc độ hay
bộ nhớ.

### EPUB3 fixed-layout — viết zip tay, không cần crate

Đủ luật, không phức tạp:

- `mimetype` là entry **đầu tiên, STORED không nén**
- OPF: `<meta property="rendition:layout">pre-paginated</meta>` ở `metadata` →
  áp cho toàn bộ spine; override từng `itemref` được nếu cần
- **mỗi XHTML là đúng một trang**, `<meta name="viewport" content="width=W, height=H">`
  khớp **đúng pixel ảnh**
- `rendition:spread` cho tranh tràn đôi trang; `page-spread-left/right` để ép
  trang bìa đứng một mình

Ảnh EPUB dùng bản 1600–2048px, **không** dùng bản in 2625px — nếu không file
sẽ ~120MB và Apple Books/Kindle sẽ khó chịu.

### Flipbook

React trong `web/`, đọc `GET /api/books/:id/pages`, ảnh qua `/api/media/:id/file`.
V2 ghép TTS của daemon thành chế độ đọc-thành-tiếng.

### Bàn giao

- `video-flow`: xuất **screenplay markdown `# Cảnh N`** — hợp đồng đã có, đừng
  chế cái khác (`rewrite-story/src/export.rs`)
- `social`: xuất ảnh trang lẻ + caption

## 8. Lộ trình

**v0 — spike, làm trước khi viết dòng code app nào.** Ba phép đo, mỗi cái vài giờ,
và cả ba đều có thể giết một nhánh thiết kế:

1. **Flow (`GEM_PIX_2`) trả ảnh bao nhiêu pixel?** Nếu < 2625 thì đường A **không
   in được** — nó tụt xuống thành đường làm nháp, và đường B thành bắt buộc.
2. ~~**Binary có Typst nặng bao nhiêu?**~~ ✅ **ĐÃ ĐO — xem §8.1.** Đạt, vừa khít.
3. **Nhất quán nhân vật qua 10 trang liền** với model sheet + style bible + seed
   cố định. Nếu không giữ được thì cả sản phẩm không có giá trị, và biết sớm ở
   ngày thứ hai tốt hơn biết ở tuần thứ sáu.

### 8.1 Kết quả spike #2 — Typst (đã chạy, 2026-08-16)

Crate thật, build thật, PDF thật: `typst-as-lib 0.16.0` + `typst`/`typst-layout`/
`typst-pdf` 0.15.1, 373 crate, cùng profile release của workspace private
(`strip`, `lto`, `codegen-units = 1`), trên macOS arm64. Nội dung test: một trang
comic 8.5×8.5 full-bleed với ảnh 2625×2625 + hai bong bóng thoại tiếng Việt, và
một trang ebook chữ tiếng Việt phủ kín bảng dấu.

**Dung lượng — con số quyết định:**

| | Binary | Zip |
|---|---|---|
| Spike Typst (gồm 1 font 773KB + ảnh test 326KB nhúng) | **29.32 MB** | **13.00 MB** |
| `ai-office` hôm nay (app đầy đủ: binary + web_dist + skills + personas) | 6.86 MB | 3.48 MB |
| **Dự phóng `picture-book`** (Typst + axum/sqlite/reqwest/MCP + web_dist) | ~34–36 MB | **~15–17 MB** |
| Trần hub | | **20 MB** |

**Lọt, nhưng chỉ còn ~3–5MB dư.** Và không trim được thêm bao nhiêu:
`typst-as-lib` **không bật default feature nào** — `reqwest`, `packages`,
`typst-html`, `typst-kit-fonts` đều tắt sẵn, nên 29.32MB đã là bản gọn nhất.
Phần nặng là code Typst, không phải phụ kiện. Hai van xả còn lại, dùng khi chạm trần:
tải font về `data_dir` lúc chạy lần đầu thay vì `include_bytes!` (~1MB/font), và
bỏ dependency `typst` full, chỉ giữ `typst-layout` cho kiểu `PagedDocument`.

**Tốc độ — không phải vấn đề, không cần nghĩ tới nữa:**

```
engine build  : 1 ms
typst compile : 10 ms   (4 trang, ảnh 2625×2625)
pdf export    : 1 ms
```

Một cuốn 40 trang render dưới 200ms. Preview realtime khi người dùng kéo bong
bóng là chuyện làm được, không phải mơ.

**PDF ra có đúng chuẩn KDP không — kiểm bằng cách đọc thẳng bytes:**

| Kiểm | Kết quả |
|---|---|
| Khổ trang | `MediaBox [0 0 630 630]` pt = **8.75 × 8.75 in chính xác** — đúng 8.5×8.5 trim + bleed 0.125 bốn cạnh |
| Phiên bản PDF | 1.7 (KDP đòi ≥ 1.4) ✅ |
| Nhúng font | 1 `FontFile`, `Type0` + `CIDFontType2` — đúng loại CID cho tiếng Việt ✅ |
| Dấu tiếng Việt | render đúng hết: *"Chị ơi, mưa ướt hết cả sách rồi!"*, *"Đừng lo — mình phơi nó ở hiên, nắng chiều sẽ hong khô thôi."* ✅ |
| Bong bóng vector | tự co theo chữ, tự wrap 2 dòng, canh giữa ✅ — đúng thứ `printpdf` bắt mình tự viết |
| Dedupe ảnh | ảnh dùng ở 2 trang → chỉ **một** XObject; 4 trang + ảnh 2625² = **357 KB** ✅ |

Cái cuối đáng chú ý hơn vẻ ngoài của nó: sách ảnh hay dùng lại một nền qua nhiều
panel, và Typst tự gộp — nghĩa là ước lượng 60–120MB ở §4.3 là **trần trên**, thực
tế sẽ thấp hơn.

**Kết luận: chốt Typst cho `render/pdf.rs`.** Chất lượng typography đạt, tốc độ dư
sức, chuẩn KDP đúng ngay từ lần render đầu. Đổi lại, app này sẽ là app nặng nhất
fleet và **mọi PR sau đó phải nhìn số zip** — nên thêm một bước đo dung lượng vào
`scripts/pack.sh` để CI kêu trước khi hub từ chối.

Template đã chạy được, chép nguyên vào `render/pdf.rs` làm điểm khởi đầu (spike
chạy trong scratchpad, không giữ lại — đây mới là thứ đáng giữ):

```typst
// Trang = 8.5" x 8.5" trim + bleed 0.125" bốn cạnh = 8.75" x 8.75".
// Margin 0, ảnh tràn viền, bong bóng là chữ VECTOR đè lên ảnh — không bao giờ
// nướng vào pixel (§4.2).
#let page_full = 8.75in
#let bleed = 0.125in
#let safe  = 0.375in            // KDP outside margin khi có bleed, sách 24-150 trang

#set page(width: page_full, height: page_full, margin: 0pt)
#set text(font: "Arial", size: 11pt, lang: "vi")

#let bubble(body, x, y, w) = place(
  dx: x, dy: y,
  block(width: w, fill: white, stroke: 1.2pt + black, radius: 8pt, inset: 10pt)[
    #set align(center); #body
  ],
)

#place(dx: 0pt, dy: 0pt, image("page.jpg", width: page_full, height: page_full))
#bubble([Chị ơi, mưa ướt hết cả sách rồi!], bleed + safe, bleed + safe, 2.6in)
#bubble([Đừng lo — mình phơi nó ở hiên, nắng chiều sẽ hong khô thôi.], 3.4in, 1.9in, 3.2in)
```

Toạ độ `x, y` của `bubble` chính là chỗ `panel.quiet_zone` (§4.2) đổ vào. Hai thứ
đó phải sinh ra từ **cùng một** quyết định bố cục, nếu không bóng thoại sẽ đè lên
mặt nhân vật — trong ảnh render thử, cả hai bóng đều rơi đúng chỗ vì được đặt tay;
lúc chạy tự động thì không ai đặt tay cả.

**v1 — ebook minh hoạ.** Một provider (Gemini API), PDF + flipbook, không panel,
không bong bóng. Đường ngắn nhất tới một cuốn sách thật cầm được.

**v1.5 — comic.** Panel grid, bong bóng vector, quiet zone, khổ vuông 8.5×8.5.

**v2** — EPUB3 FXL, đường A (extension), bàn giao video-flow/social, TTS đọc.

## 9. Không làm

- **Không sinh chữ vào ảnh** (§4.2). Luật, không phải sở thích.
- **Không tự upload lên KDP.** KDP không có API xuất bản công khai, và xuất bản
  là hành động thay mặt người dùng ra thế giới — app dừng ở file, người dùng bấm nút.
- **Không sinh nhân vật có bản quyền.** Cần chốt chặn ngay ở stage `cast`, không
  phải ở lúc export — lúc đó đã tốn tiền ảnh rồi.
- **Không CMYK.** KDP nhận sRGB cho ruột màu; chuyển CMYK là tự chuốc lệch màu.
- **Không chunk đều thay cho beat** (§4.4).

## 10. Câu hỏi còn mở

1. **`image.request` ở daemon — có làm không?** Đây là câu hỏi kiến trúc lớn nhất
   của tài liệu này. Có thì §5 sạch sẽ và `video-flow` cũng thoát khỏi việc phải
   mở Chrome để sinh ảnh tham chiếu. Không thì `picture-book` là app đầu tiên
   trong fleet giữ provider key, và cần ghi rõ đó là ngoại lệ có chủ đích.
2. **Khổ mặc định**: chốt 8.5×8.5 cho comic (khớp 1:1) và 6×9 cho ebook?
3. **Sách tiếng Việt hay song ngữ?** Ảnh dùng chung được cho mọi ngôn ngữ **chính
   vì** §4.2 — nếu có ý định bán ở nhiều thị trường thì đây là lý do thứ sáu để
   giữ luật đó.
4. **Font**: cần font comic hỗ trợ đủ dấu tiếng Việt, giấy phép cho phép nhúng và
   bán thương mại. Không nhiều font comic làm được cả ba.

---

## Nguồn

- [KDP — Set Trim Size, Bleed, and Margins](https://kdp.amazon.com/en_US/help/topic/GVBQ3CMEQW3W2VL6)
- [KDP — Paperback Submission Guidelines](https://kdp.amazon.com/en_US/help/topic/G201857950)
- [EPUB 3 Fixed Layout Documents (IDPF)](https://idpf.org/epub/fxl/)
- [Apple — EPUB 3 Fixed Layout](https://help.apple.com/itc/booksassetguide/en.lproj/itcef2bad6b8.html)
- [Google — Introducing Gemini 2.5 Flash Image](https://developers.googleblog.com/introducing-gemini-2-5-flash-image/)
- [Google Gemini API pricing guide 2026 — Curlscape](https://curlscape.com/blog/google-gemini-api-pricing-guide-2026)
- [typst-as-lib — crates.io](https://crates.io/crates/typst-as-lib)
- [Typst — Automated PDF Generation](https://typst.app/blog/2025/automated-generation/)
- [printpdf — GitHub](https://github.com/fschutt/printpdf)

Trong repo: `apps/video-flow/src/process.rs` (nhất quán nhân vật, cầu Flow),
`apps/rewrite-story/src/text.rs` + `src/export.rs` (chia tiếng Việt, hợp đồng bàn
giao), `apps/video-flow/src/tts.rs` (TTS của daemon),
`SemaClaw/src/gateway/ui_server/space.rs:2151-2540` (bridge action nào chạy thật).
