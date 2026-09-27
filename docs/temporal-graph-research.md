# Temporal Graph cho Knowledge Graph của SenClaw — Nghiên cứu & Thiết kế

> Đo trên DB thật `~/.senclaw/senclaw_cognitive.db` ngày 2026-08-08
> (5.634 nodes / 21.112 edges, dữ liệu 21/07 → 04/08/2026).

## Trạng thái triển khai (2026-08-08)

| Phase | Trạng thái | Ghi chú |
|---|---|---|
| **P0** tách `archived_at` khỏi `valid_to` | **ĐÃ LÀM** | + repair chạy **mỗi lần boot**, không phải một lần — xem §3.1 |
| **P1** supersession theo cardinality | **ĐÃ LÀM** | bảng `cog_predicate_meta`, seed 33 single / 14 multi |
| **P3** truy vấn `as_of` + `SearchType::Temporal` + `cog_history` | **ĐÃ LÀM** | xuyên retriever → MCP → REST → tool của agent |
| **P2** trích thời gian sự kiện lúc ingest | chưa | `valid_from` vẫn = thời gian ingest (xem §3.4) |
| **P4** UI thanh trượt thời gian | chưa | API đã trả `validFrom`/`validTo`/`archivedAt` sẵn cho UI |
| **P5** predicate `observation` (không đẻ entity cho từng con số) | chưa | 550 entity số vẫn còn đó |

Hai điều học được **khi code** mà lúc thiết kế không thấy, đã ghi vào đúng mục
bên dưới: (1) lọc cạnh theo thời gian là **chưa đủ** — §3.5; (2) migration một
lần là **chưa đủ** khi daemon bản cũ còn chạy — §3.1.

## 0. Tóm tắt

Đồ thị tri thức trả lời được "A liên quan tới B", nhưng **chưa trả lời được "A
còn đúng không, và đúng từ bao giờ"**. Đó chính là trục mà một kiến trúc graph
vẫn còn "phẳng" y như một cái timeline: nếu mỗi fact không mang khoảng hiệu lực,
thì fact cũ và fact mới cùng nằm trong đồ thị với tư cách ngang nhau, và
retrieval chọn theo *độ giống* chứ không theo *độ đúng ở hiện tại*.

Đo được trên DB thật: **giá vàng BTMC có ba `sell_price` cùng tồn tại**
(149.900 ngày 30/07 → 141.500 ngày 03/08 → 140.000 ngày 04/08), không cạnh nào
biết mình đã bị thay thế. Hỏi "giá vàng BTMC bao nhiêu" thì thứ quyết định câu
trả lời là điểm vector + `strength`, không phải thời gian.

Đề xuất: **bi-temporal** đúng nghĩa (thời gian thế giới ↔ thời gian hệ thống),
và điều then chốt là **tách cột `valid_to` đang bị dùng làm marker archive ra
khỏi ngữ nghĩa "hết đúng"**. Phần lớn hạ tầng đã có sẵn — cột `valid_from` /
`valid_to` nằm trong `cog_edges` từ đầu, chỉ là ngữ nghĩa đã trôi.

---

## 1. Hiện trạng — đo được, không phỏng đoán

### 1.1 Bảy phát hiện

| # | Phát hiện | Bằng chứng |
|---|---|---|
| 1 | `valid_from` **là thời gian ingest**, không phải thời gian sự kiện | `RelationshipEdge::new` gán `valid_from: now` ([triplet.rs:57](../src/memory/cognitive/triplet.rs)). SQL: `SELECT count(*) FROM cog_edges WHERE valid_from <> created_at` → **0/21.112** |
| 2 | `valid_to` **bị chiếm dụng làm marker archive** của decay, không phải "hết đúng" | `archive()` set `valid_to = now` ([triplet.rs:202](../src/memory/cognitive/triplet.rs)); `is_archived()` đọc `valid_to.is_some()` |
| 3 | Marker đó đang phủ **96% đồ thị** | 20.282 archived / 830 active |
| 4 | `strengthen()` **xoá `valid_to`** | [triplet.rs:116](../src/memory/cognitive/triplet.rs) — nếu dùng `valid_to` làm invalidation thì *một lần nhắc lại* sẽ hồi sinh fact đã sai |
| 5 | Extraction **mù thời gian** | `SYSTEM_PROMPT` ([cognify.rs:50](../src/memory/cognitive/cognify.rs)) không có trường thời gian nào trong schema JSON |
| 6 | Không có bước **thay thế fact cũ** | `upsert_triplet` chỉ tìm cạnh trùng `(subj, obj, predicate)` ([cognify.rs:486](../src/memory/cognitive/cognify.rs)); object khác ⇒ cạnh mới, cạnh cũ vẫn active |
| 7 | Toàn bộ mặt truy vấn **không có tham số thời gian** | `SearchQuery` ([search.rs:34](../src/memory/cognitive/search.rs)) không có `as_of`; 18 route `/api/cognitive/*` không nhận `as_of`; chính docstring của `SearchType` ghi `TEMPORAL` thuộc "later phases" ([search.rs:4](../src/memory/cognitive/search.rs)) |

### 1.2 Bằng chứng: fact biến thiên đang chồng lên nhau

```
Bảo Tín Minh Châu (BTMC SJC)  sell_price  149.900   ingest 2026-07-30 17:29
Bảo Tín Minh Châu (BTMC SJC)  sell_price  141.500   ingest 2026-08-03 06:14
Bảo Tín Minh Châu (BTMC SJC)  sell_price  140.000   ingest 2026-08-04 02:17
```

Ba cạnh, ba object khác nhau, cùng một `(src, predicate)`. Không cạnh nào có
`valid_to` mang nghĩa "bị thay thế lúc 03/08" — cả ba đều bị decay archive vào
những thời điểm chẳng liên quan gì tới việc giá thay đổi. Cùng dạng: `price`
của Bitcoin (13 giá trị trong 4 ngày), `buy_price`, `has_status`, `has_uptime`,
`has_used_ram`.

Thống kê ứng viên mâu thuẫn (cùng `src` + `predicate`, nhiều `dst`):

| predicate | số subject bị đa trị | số cạnh liên quan |
|---|---|---|
| `ASSOCIATED_WITH` | 490 | 2.882 |
| `is_a` | 450 | 1.070 |
| `includes` | 66 | 216 |
| `uses` | 24 | 75 |
| `sell_price` / `buy_price` / `price` | 16 / 16 / 5 | 45 / 45 / 22 |
| `has_task` | — | 34 |

Quan trọng: **không phải đa trị nào cũng là mâu thuẫn.** `includes` (một danh
mục gồm 5 sản phẩm) và `has_task` (một người có 3 việc) là đa trị hợp lệ;
`sell_price` thì không. Đây là lý do thiết kế bên dưới đặt *cardinality của
predicate* làm trung tâm chứ không phải một cú gọi LLM cho mỗi triplet.

### 1.3 Phát hiện phụ (ngoài phạm vi nhưng liên quan trực tiếp)

**550/4.750 entity (11,6%) là con số**: `khoảng 770.000đ`, `1.669.595.366 VNĐ`,
`26.254 - 26.260 VNĐ`… Mỗi lần cognify một bản tin giá là một entity mới vĩnh
viễn. Đây vừa là nguồn phình đồ thị vừa là dấu hiệu cho thấy nhóm predicate
"đo lường theo thời gian" cần một cách lưu khác — xem §5.3.

---

## 2. Prior art

| Hệ | Mô hình thời gian | Điều đáng lấy |
|---|---|---|
| **Graphiti / Zep** | Bi-temporal đầy đủ: `valid_at`/`invalid_at` (thế giới) + `created_at`/`expired_at` (hệ thống) | Mâu thuẫn ⇒ **ghi `t_invalid`, không xoá**. Trùng khớp triết lý archive-not-delete của SenClaw |
| **cognee** | "Temporal cognification": trích mốc thời gian ngay lúc ingest, timestamp thành node riêng, quan hệ `before`/`after`/`during`; `SearchType.TEMPORAL` | Trích thời gian **tại tầng extraction** (chỗ SenClaw đang trống) |
| **Bitemporal memory store** (arXiv 2607.26520) | `:Memory` bất biến + `:MemoryVersion` có khoảng `[valid_from, valid_to)`; hai chỉ mục vector: current-only và all-versions | Bài học đắt: bật time-travel làm **giảm** recall trên câu hỏi thời gian (50% → 37,5%) do over-fetch bản cũ. ⇒ as-of phải **opt-in**, mặc định luôn là "hiện tại" |

Điểm chung của cả ba: *không xoá*, chỉ đóng khoảng hiệu lực. SenClaw đã có sẵn
văn hoá đó ở decay (archive thay vì prune) — temporal chỉ là áp cùng nguyên tắc
lên trục thứ hai.

---

## 3. Thiết kế đề xuất

### 3.1 Bốn mốc thời gian, hai trục

| Cột | Trục | Nghĩa | Trạng thái |
|---|---|---|---|
| `valid_from` | thế giới | fact bắt đầu đúng | **có sẵn** — chỉ cần được ghi đúng (hiện = ingest) |
| `valid_to` | thế giới | fact hết đúng (bị fact mới thay thế) | **có sẵn** — cần *giải phóng* khỏi vai trò marker archive |
| `created_at` | hệ thống | lúc hệ thống học được | có sẵn |
| `archived_at` | hệ thống | lúc decay cho ngủ đông | **cột mới** |
| `invalidated_by` | provenance | chunk/episode đã lật đổ fact này | cột mới, nullable |

Điểm mấu chốt là hoán đổi ngữ nghĩa chứ không phải thêm khái niệm: comment
trong schema vốn đã ghi `-- temporal validity (cognee temporal awareness)`
([schema.rs:130](../src/memory/cognitive/schema.rs)) — code chỉ trôi khỏi ý định
ban đầu khi decay mượn tạm cột này làm marker.

Migration **không mơ hồ**, vì hôm nay `archive()` là *nơi duy nhất* ghi
`valid_to`:

```sql
ALTER TABLE cog_edges ADD COLUMN archived_at INTEGER;
ALTER TABLE cog_edges ADD COLUMN invalidated_by BLOB;
UPDATE cog_edges
   SET archived_at = COALESCE(archived_at, valid_to), valid_to = NULL
 WHERE valid_to IS NOT NULL AND invalidated_by IS NULL;
CREATE INDEX idx_cog_edges_current ON cog_edges(src, predicate) WHERE valid_to IS NULL;
```

**Sửa so với thiết kế ban đầu — repair phải chạy MỖI LẦN BOOT, không phải một
lần.** Bản đầu tôi viết migration một chiều gate bằng "cột chưa tồn tại", và nó
sai ngay trong lúc phát triển: một daemon build **trước** bản tách vẫn tiếp tục
ghi marker archive vào `valid_to`, mà luôn có cửa sổ hai bản chạy song song —
update desktop tại chỗ, một `cargo test` lạc, MCP subprocess của bundle cũ. Khi
bản mới boot lại, đống marker đó sẽ bị đọc thành "fact đã bị thay thế" và **biến
mất khỏi mọi truy vấn hiện tại**.

Phân biệt được **chính xác**, không phải đoán: supersession luôn ghi
`invalidated_by` cùng lúc với `valid_to` (`RelationshipEdge::invalidate`), nên
`valid_to IS NOT NULL AND invalidated_by IS NULL` chỉ có thể do decay bản cũ tạo
ra. Đó là mệnh đề `WHERE` ở trên, và nó tự lành mọi lần boat kế tiếp.
(Đo thực tế trên máy này: 20.477 marker đã chuyển, rồi daemon bản cũ ghi thêm
7.043 marker nữa trong lúc đang phát triển — repair nuốt gọn cả hai đợt.)

Kéo theo trong `triplet.rs`: `is_archived()` đọc `archived_at`; `archive()` ghi
`archived_at`; `strengthen()` xoá `archived_at` **và không bao giờ đụng
`valid_to`** — một fact sai được nhắc lại vẫn là fact sai.

`scan_edges` (decay) hiện lọc `valid_to IS NULL` ([graph_store.rs:829](../src/memory/cognitive/graph_store.rs))
→ đổi thành `archived_at IS NULL`; nếu quên, sau migration decay sẽ quét lại
toàn bộ 20k cạnh đã ngủ.

### 3.2 Phát hiện mâu thuẫn: luật trước, LLM sau

Bảng mới `cog_predicate_meta(predicate PK, cardinality, temporal_kind, source)`:

| `cardinality` | Nghĩa | Khi có fact mới |
|---|---|---|
| `single` | mỗi subject đúng một object (`sell_price`, `has_status`, `name`, `lives_in`) | đóng `valid_to` các cạnh cũ cùng `(src, predicate)` |
| `multi` | đa trị hợp lệ (`includes`, `has_task`, `uses`, `MENTIONS`, `is_a`) | không làm gì |
| `unknown` | chưa biết | mặc định `multi` (an toàn: thà giữ thừa còn hơn giết nhầm), xếp hàng cho LLM phán một lần rồi ghi lại |

Seed ban đầu lấy từ chính dữ liệu đo được ở §1.2 chứ không bịa. Quy tắc đóng
khoảng, đặt trong `upsert_triplet`:

```
khi thêm cạnh (s, p, o_mới) với p.cardinality == single:
    với mỗi cạnh active (s, p, o_cũ), o_cũ ≠ o_mới:
        nếu valid_from(o_cũ) <= valid_from(o_mới):        # chống ingest lệch thứ tự
            valid_to(o_cũ)      = valid_from(o_mới)
            invalidated_by(o_cũ) = chunk_id của fact mới
```

Điều kiện `valid_from(cũ) <= valid_from(mới)` là cái chặn ca **nạp tư liệu lịch
sử**: import một bài báo năm 2019 không được phép lật đổ fact của hôm nay.
Không có nó thì mỗi lần upload tài liệu cũ là một lần đồ thị "quay ngược".

Chi phí: **0 token**. LLM chỉ chạm vào predicate lạ, một lần cho mỗi predicate,
kết quả ghi vào bảng — không phải mỗi triplet.

### 3.3 Predicate đo lường (`temporal_kind = observation`)

`price`, `has_uptime`, `has_used_ram`, `sell_price` không phải "fact bị thay
thế" mà là **chuỗi thời gian**. Đóng khoảng cho chúng vẫn đúng, nhưng vẫn để lại
550 entity kiểu `1.669.595.366 VNĐ`.

Đề xuất tối thiểu (P2, có thể tách riêng): với `observation`, giữ **N mốc gần
nhất** ở dạng cạnh, phần còn lại gộp vào `props_json` của cạnh hiện hành dưới
dạng chuỗi `(t, value)` — đồ thị giữ được "giá vàng đang là X, tuần qua đi từ Y
đến Z" mà không sinh entity cho từng con số. Không làm phần này thì temporal vẫn
chạy đúng, chỉ là đồ thị tiếp tục phình.

### 3.4 Trích thời gian sự kiện lúc ingest

Thêm hai trường tuỳ chọn vào schema JSON của `SYSTEM_PROMPT`:

```json
{"subject":"…","predicate":"…","object":"…",
 "valid_at":"2026-08-04 | hôm qua | từ 2019 | null",
 "valid_until":"… | null"}
```

Kèm một dòng reference time trong user prompt (`build_user_prompt`, [cognify.rs:216](../src/memory/cognitive/cognify.rs)):
chunk đã có `created_at`, đó là mốc để quy chiếu.

Phía Rust: parser nhỏ cho biểu thức tương đối tiếng Việt/Anh
(`hôm qua`, `tuần trước`, `từ tháng 3`, `since 2019`) → epoch; parse thất bại ⇒
`valid_at = NULL` ⇒ fallback về `created_at` (đúng bằng hành vi hôm nay, nên
không có hồi quy). **Không** để LLM tự trả epoch: model nhỏ bịa số rất giỏi.

Bẫy múi giờ: chuẩn hoá về UTC ngay tại parser, vì toàn bộ storage là
unix-seconds (schema.rs dòng 18) còn văn bản tiếng Việt thì luôn là giờ VN.

> **Chưa làm (P2).** `valid_from` hiện vẫn là thời gian ingest. Với memory chạy
> theo dòng chat/ingest thì đó là xấp xỉ hợp lý và là đúng thứ dữ liệu hôm nay
> đang có, nên toàn bộ máy móc thời gian ở trên vẫn chạy đúng — chỉ là "lúc hệ
> thống biết" chứ chưa phải "lúc fact bắt đầu đúng". Riêng `parse_as_of` (đọc
> `as_of` từ người dùng) thì **đã** có và đã theo đúng luật múi giờ trên: chuỗi
> không có timezone được đọc là **giờ địa phương**, `2026-07-31` trần nghĩa là
> hết ngày 31/07.

### 3.5 Mặt truy vấn as-of

```rust
pub struct SearchQuery {
    …
    /// None = fact đang đúng (mặc định). Some(t) = "đúng tại thời điểm t".
    pub as_of: Option<i64>,
    /// Some(t) = "hệ thống tin gì tại t" (audit, khác hẳn as_of)
    pub as_known_at: Option<i64>,
}
```

Bộ lọc, đặt ở tầng `retrievers` (nơi đã tính `effective_strength`):

| Chế độ | Điều kiện SQL |
|---|---|
| hiện tại (mặc định) | `valid_to IS NULL` |
| as-of `T` (thời gian thế giới) | `valid_from <= T AND (valid_to IS NULL OR valid_to > T)` |
| as-known-at `T` (thời gian hệ thống) | `created_at <= T AND (invalidated_at IS NULL OR invalidated_at > T)` |

Theo đúng bài học của arXiv 2607.26520: khi `as_of` được set thì over-fetch
rồi post-filter, **và cộng thêm một hạng tử ưu tiên gần `as_of`** — không có nó,
time-travel làm loãng kết quả và recall tụt như họ đo được. Trong
`temporal_proximity` ([retrievers.rs](../src/memory/cognitive/retrievers.rs)),
độ gần thời gian chiếm 0.85 và `strength` chỉ còn 0.15 để phá hoà: một cái giá
được nhắc trăm lần vẫn không phải là giá của ngày đang hỏi.

**Bổ sung sau khi code — lọc cạnh là CHƯA ĐỦ.** Test bắt được ngay: hỏi "BTMC"
vẫn ra `149.900` dù cạnh đã bị đóng. Lý do là bước **seed** không đi qua cạnh
nào cả — nó là vector/FTS trên **node**, mà `149.900` vẫn là một node hợp lệ.
Fact cũ đi thẳng vào kết quả với tư cách hit trực tiếp.

Vá tại một chỗ duy nhất (`CognitiveRetriever::retain_temporally_visible`, chạy
sau mọi mode): **bỏ hit nào CÓ cạnh nhưng KHÔNG cạnh nào còn hiệu lực tại thời
điểm đang hỏi**. Luật hẹp có chủ đích — node *không có* cạnh nào (entity vừa
trích, chunk văn bản) không phải quá khứ, nó chỉ chưa có fact; ẩn nó đi là làm
hỏng recall văn bản thường. Giá: một truy vấn có index cho mỗi hit sống sót.

Bề mặt đã phơi ra (đã code):

- `SearchType::Temporal` — điền đúng cái ô mà docstring đã chừa sẵn.
- MCP `senclaw-cognitive`: `cog_search` / `cog_recall` thêm `as_of`; thêm
  **`cog_history(subject, predicate)`** trả về dòng thời gian của một fact
  ("sell_price = 149.900 [30/07 → 03/08], = 141.500 [03/08 → nay] (current)").
- Tool in-process của agent: `CogSearch` thêm mode `temporal` + `as_of`,
  `CogRecall` thêm `as_of`.
- REST: `asOf` trong body của `/api/cognitive/search` và `/recall`;
  `GET /api/cognitive/history?subject=&predicate=`;
  `GET|POST /api/cognitive/predicates` để xem/sửa registry cardinality.
  `EdgeView` giờ trả `validFrom` / `validTo` / `archivedAt`.
- **`as_of` không đọc được là LỖI, không phải im lặng trả về "bây giờ"** — ở cả
  ba tầng (tool, MCP, REST). Trả lời về hiện tại khi người ta hỏi về quá khứ là
  câu trả lời sai đội lốt đúng.

Còn lại cho P4: UI thanh trượt thời gian; panel node gạch ngang fact hết hiệu
lực kèm "bị thay thế bởi …" (API đã đủ dữ liệu).

### 3.6 Quan hệ với decay/archive

Hai trục độc lập, và phải giữ cho chúng độc lập:

| | `archived_at` (hệ thống) | `valid_to` (thế giới) |
|---|---|---|
| Ai ghi | decay tick | phát hiện mâu thuẫn (§3.2) |
| Nghĩa | ít dùng đến, ngủ đông | không còn đúng |
| `strengthen()` | **xoá** (đánh thức) | **không đụng tới** |
| Retrieval | vẫn duyệt, weight thấp | ẩn khỏi truy vấn hiện tại; chỉ hiện khi as-of |

Nhắc lại một fact sai không làm nó đúng lại — đó chính là con bug mà việc tách
cột này ngăn chặn, và cũng là lý do không thể "tận dụng luôn `valid_to`".

---

## 4. Rủi ro & bẫy

1. **Migration một chiều.** Sau khi `UPDATE cog_edges SET valid_to = NULL`, dữ
   liệu archive chỉ còn ở `archived_at`; bản daemon cũ chạy trên DB mới sẽ coi
   20k cạnh ngủ đông là active và decay quét lại từ đầu. ⇒ bump schema version,
   và test rollback.
2. **Time-travel làm giảm recall** (bằng chứng đo được của arXiv 2607.26520:
   50% → 37,5%). ⇒ `as_of` không bao giờ là mặc định, kể cả trong prompt injection.
3. **Ingest lệch thứ tự** (tài liệu cũ nạp sau) — đã chặn bằng điều kiện
   `valid_from(cũ) <= valid_from(mới)`, nhưng cần test riêng vì đây là ca dễ
   quên nhất.
4. **Cardinality đoán sai** giết fact hợp lệ. Vì vậy `unknown` mặc định `multi`,
   và invalidation **archive-not-delete** đúng như [[cognitive-archive-not-delete]]
   — sai thì sửa được bằng cách xoá `valid_to`.
5. **Predicate không chuẩn hoá.** LLM sinh `sell_price`, `giá bán`, `price`
   cho cùng một ý ⇒ registry phải khớp sau khi lowercase + có bảng alias, nếu
   không thì luật single-cardinality trượt trong im lặng.
6. **Chi phí LLM.** Không gọi LLM cho mỗi triplet (21k cạnh/2 tuần ⇒ không kham
   nổi); chỉ cho predicate lạ, một lần, có cache trong bảng.
7. **Đồ thị 96% archived.** Trước khi đo hiệu quả temporal, nên xem lại
   `max_age` của L2 (30 ngày) so với nhịp thực tế — hiện gần như mọi thứ đều
   ngủ, nên "current fact" và "active edge" đang là hai tập gần rời nhau.

---

## 5. Kế hoạch triển khai

| Phase | Nội dung | Đụng vào | Ước lượng |
|---|---|---|---|
| **P0** | Tách `archived_at` khỏi `valid_to` + migration + sửa `scan_edges` | `schema.rs`, `triplet.rs`, `graph_store.rs`, `decay_tick.rs` | nhỏ, rủi ro tập trung ở migration |
| **P1** | `cog_predicate_meta` + đóng khoảng theo cardinality trong `upsert_triplet` | `cognify.rs`, `graph_store.rs` | vừa |
| **P2** | Trích `valid_at` ở extraction + parser thời gian tương đối | `cognify.rs`, module `temporal_parse` mới | vừa |
| **P3** | `as_of` xuyên `SearchQuery` → retrievers → MCP → REST; `SearchType::Temporal`; `cog_history` | `search.rs`, `retrievers.rs`, `cognitive_server.rs`, `ui_server/cognitive.rs` | vừa |
| **P4** | UI: thanh trượt thời gian, gạch ngang fact hết hạn, timeline của một fact | `desktop_app`, `web` | vừa |
| **P5 (tuỳ chọn)** | `observation` predicate: không sinh entity cho từng con số | `cognify.rs` | tách riêng được |

P0 độc lập và **nên làm trước bất kể có làm temporal hay không** — hiện tại
`valid_to` mang hai nghĩa là một quả mìn hẹn giờ cho bất kỳ ai đọc schema và tin
vào cái comment "temporal validity".

### Test bắt buộc

- Migration: 20.282 dòng archived chuyển đúng sang `archived_at`, `valid_to`
  sạch; chạy hai lần idempotent.
- `strengthen()` đánh thức cạnh ngủ **nhưng không** hồi sinh cạnh đã invalid.
- Đóng khoảng: `sell_price` 149.900 → 141.500 làm cạnh cũ có `valid_to` đúng
  bằng `valid_from` của cạnh mới; `includes`/`has_task` (multi) **không** bị đụng.
- Ingest lệch thứ tự: fact năm 2019 nạp sau fact 2026 không lật đổ được.
- as-of: truy vấn tại 31/07 trả 149.900, tại 04/08 trả 140.000, mặc định trả
  140.000.
- Không hồi quy: mọi query hiện tại không truyền `as_of` cho kết quả y hệt
  trước migration.

## 6. Không làm (non-goals)

- Đại số khoảng Allen (`before`/`overlaps`/`during`) và suy luận thời gian đa
  bước — cognee có, nhưng chi phí/lợi ích chưa xứng khi ta còn chưa biết fact
  nào đang đúng.
- Temporal GNN / học biểu diễn theo thời gian: `gnn_sage.rs` đã có sẵn hạ tầng,
  nhưng đây là bài toán khác.
- Timestamp-thành-node như cognee: với SQLite, cột + index rẻ hơn nhiều so với
  nhân số node lên.
- Gọi LLM để phân xử từng mâu thuẫn.

## Nguồn

- [Graphiti: Knowledge graph memory for an agentic world — Neo4j](https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/)
- [What Is a Temporal Knowledge Graph? — Zep](https://www.getzep.com/ai-agents/temporal-knowledge-graph/)
- [Zep: Temporal Knowledge Graph Architecture — Emergent Mind](https://www.emergentmind.com/topics/zep-a-temporal-knowledge-graph-architecture)
- [Temporal Cognification — cognee](https://www.cognee.ai/blog/cognee-news/unlock-your-llm-s-time-awareness-introducing-temporal-cognification)
- [Time Awareness — cognee docs](https://docs.cognee.ai/guides/time-awareness)
- [A Graph-Native Bitemporal Memory Store for Conversational AI Agents (arXiv 2607.26520)](https://arxiv.org/html/2607.26520v1)
- [Building Temporal Knowledge Graphs with Graphiti — FalkorDB](https://www.falkordb.com/blog/building-temporal-knowledge-graphs-graphiti/)
