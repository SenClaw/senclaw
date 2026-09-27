# Phân tích lát cắt sự kiện 24h ↔ 24h (app Tin Tức)

> **Trạng thái: ĐỀ XUẤT ĐÓNG — chờ người dùng chốt** · nêu 2026-09-12.
> Lý do đề xuất: app `news` đã chuyển sang repo `senclaw-app`, nên tài liệu
> này nằm sai repo. **Chưa có dòng code nào ở đây.**


Nghiên cứu phương pháp so sánh **cửa sổ 24h hiện tại (T)** với **24h liền trước
(T‑1)** ở **mức SỰ KIỆN** (story), để trả lời hai câu hỏi:

1. **Biến động** — sự kiện nào mới nổi, bùng phát, hạ nhiệt, tắt hẳn?
2. **Liên quan** — các sự kiện trong lát cắt dính với nhau thế nào, cái nào đi
   trước cái nào?

Trạng thái: **nghiên cứu + thiết kế, CHƯA cài đặt.** Mọi số liệu dưới đây đo
thật trên `~/.senclaw/apps/news/news.db` ngày 2026‑07‑29.

---

## 1. Đã có gì, thiếu gì

| Thành phần | Có sẵn | Mức | Có so 2 cửa sổ? |
|---|---|---|---|
| `detect_trends` (cluster.rs) | ✅ | **cụm từ** (n‑gram 1–3) | ✅ T vs T‑1 |
| `assign_story` / `stories` | ✅ | **sự kiện** | ❌ không có khái niệm cửa sổ |
| `story_links` + graph | ✅ | **sự kiện ↔ sự kiện** | ❌ tĩnh, không theo thời gian |
| `news_analyze_graph` (AI) | ✅ | ngữ nghĩa | ❌ |

→ **Khoảng trống chính xác cần lấp: diff theo cửa sổ ở mức SỰ KIỆN, và quan hệ
giữa các sự kiện có yếu tố THỜI GIAN.** Trends đã làm đúng việc đó nhưng ở mức
cụm từ; một cụm từ không phải một sự kiện.

---

## 2. Đo thực tế trước khi thiết kế

### 2.1 Khối lượng theo cửa sổ

| Cửa sổ | Bài | Story | Nguồn |
|---|---|---|---|
| T (0–24h) | 2 113 | 1 729 | 61 |
| T‑1 (24–48h) | 515 | 451 | 31 |
| T‑2 (48–72h) | 266 | 207 | 19 |

### 2.2 Phân bố kích thước sự kiện trong T

| Tổng story | ≥2 bài | ≥3 bài | ≥5 bài | ≥2 nguồn | ≥3 nguồn |
|---|---|---|---|---|---|
| 1 729 | **95** | 42 | 24 | **74** | 30 |

### 2.3 Lát cắt thô của 8 sự kiện lớn nhất

| Sự kiện | n(T) | n(T‑1) | nguồn |
|---|---|---|---|
| Vụ nổ ở TTTM Aeon Mall | 57 | 14 | 16 |
| Tô Lâm tiếp Chủ tịch Quốc hội Campuchia | 37 | 9 | 12 |
| Làn trái cao tốc và 'luật mềm' vượt xe | 18 | 2 | 7 |
| Việt Nam tìm hiểu mô hình Hàn Quốc | 18 | 3 | 7 |
| Saudi Arabia, US strikes on Iran‑backed… | 15 | 1 | 12 |
| Chuyển Bộ Công an điều tra sai phạm | 14 | 4 | 10 |
| Giá vàng hôm nay 29.7 | 13 | 4 | 7 |
| Nghiên cứu khu kinh tế đặc biệt | 13 | 2 | 7 |

---

## 3. Ba cái bẫy mà số liệu trên vạch ra

Đây là phần quan trọng nhất của nghiên cứu: **nếu làm ngây thơ, cả ba đều tạo ra
"biến động" hoàn toàn giả.**

### Bẫy 1 — Trôi khối lượng (volume drift). NGHIÊM TRỌNG NHẤT.

T có 2 113 bài / 61 nguồn, T‑1 có 515 bài / 31 nguồn. Tỷ lệ 4,1×. Nhưng **không
có tin gì bùng nổ cả** — chỉ là vừa thêm 58 nguồn vào lúc T bắt đầu.

Xem lại Aeon Mall: 14 → 57 = **4,07×**, đúng bằng tỷ lệ trôi khối lượng toàn
cục. Nghĩa là sau khi chuẩn hoá, sự kiện này **đứng yên**, không hề "bùng phát".
Công thức thô `score = n·(n+1)/(prev+1)` mà `detect_trends` đang dùng sẽ chấm nó
2 300 điểm và đẩy lên đầu bảng — sai hoàn toàn.

**Cách chữa đúng (khuyến nghị): so trên GIAO của tập nguồn hoạt động ở CẢ HAI cửa
sổ.** Chuẩn hoá theo tổng khối lượng toàn cục cũng khử được phần ngoại sinh,
nhưng nó khử luôn cả phần tăng THẬT của một ngày tin nóng. Lọc theo giao tập
nguồn chỉ bỏ đi phần ngoại sinh, giữ nguyên tín hiệu thật.

```sql
-- chỉ những nguồn có bài ở CẢ hai cửa sổ mới được vào phép so
WITH src_both AS (
  SELECT source_id FROM articles WHERE published_at>=:t0 AND published_at<:t1
  INTERSECT
  SELECT source_id FROM articles WHERE published_at>=:p0 AND published_at<:p1
)
```

### Bẫy 2 — 21,7% bài KHÔNG có ngày đăng thật

`published_at == fetched_at` ở 571/2 628 bài 48h gần đây. Feed thiếu `pubDate`,
hoặc trang scrape không có `article:published_time` → `insert_article` thay bằng
thời điểm quét. Những bài này **dồn hết vào "bây giờ"**, tạo đột biến giả ở đầu
cửa sổ T mỗi lần quét.

Chữa: thêm cột `has_real_date` (hoặc suy ra `published_at != fetched_at`) và
**loại khỏi phép tính lát cắt** — hoặc ít nhất báo tỷ lệ này kèm kết quả để người
đọc biết độ tin cậy. Đây là thay đổi schema nhỏ, nên làm trước.

### Bẫy 3 — 95% story là singleton

1 729 story nhưng chỉ **95 có ≥2 bài**, **74 có ≥2 nguồn**. Nếu lát cắt chạy trên
toàn bộ story, 94,5% đầu vào là nhiễu một‑bài‑một‑story.

Chữa: **quần thể phân tích = story có ≥2 bài VÀ ≥2 nguồn trong cửa sổ T ∪ T‑1**
(~95 sự kiện). Đây là con số đẹp: đủ nhiều để có ý nghĩa, đủ ít để chạy O(n²)
thoải mái.

---

## 4. Phương pháp đề xuất

### 4.1 Dựng lát cắt

Với mỗi story `s` và mỗi cửa sổ `W ∈ {T, T-1}`, tính **trên giao tập nguồn**:

- `n_W(s)` — số bài
- `src_W(s)` — số nguồn phân biệt (**độ lan**, tín hiệu quý nhất — 10 bài từ 1
  nguồn ≠ 10 bài từ 10 nguồn)
- `h_W(s)[0..23]` — histogram theo giờ (`published_at / 3600`)

Tuyệt đối **không dùng `stories.article_count`** — đó là tổng cả đời của story,
không phải của cửa sổ.

### 4.2 Chấm biến động

Khuyến nghị **log‑odds‑ratio với tiên nghiệm Dirichlet** (Monroe, Colaresi &
Quinn 2008) — đây là phương pháp chuẩn cho bài toán "mục nào phân biệt kho A với
kho B", và quan trọng là **xử lý đúng đếm nhỏ và đếm bằng 0**, thứ mà tỷ lệ thô
làm rất tệ:

```
δ(s) = log( (n_T + α) / (N_T + α₀ − n_T − α) )
     − log( (n_P + α) / (N_P + α₀ − n_P − α) )

var(δ) ≈ 1/(n_T + α) + 1/(n_P + α)
z(s)   = δ / √var(δ)
```

`α` lấy từ tần suất gộp hai cửa sổ (tiên nghiệm thông tin), `α₀ = Σα`.

Phương án đơn giản hơn nếu muốn ít code: **z‑score Poisson**

```
λ(s) = n_P(s) · r        với r = N_T / N_P  (tính trên giao tập nguồn)
z(s) = (n_T(s) − λ(s)) / √(λ(s) + 0.5)
```

Cả hai đều cho z có thể đặt ngưỡng, khác hẳn `score` hiện tại (không có thang).

**Cổng độ lan:** chỉ báo biến động khi `src_T(s) ≥ 2`. Một nguồn đăng 20 bài về
chính nó không phải sự kiện.

### 4.3 Phân loại

| Nhãn | Điều kiện |
|---|---|
| **MỚI NỔI** | `n_P = 0`, `n_T ≥ 3`, `src_T ≥ 2` |
| **BÙNG PHÁT** | `z ≥ 2`, `n_T ≥ 3` |
| **DUY TRÌ** | `|z| < 2`, `n_T ≥ 2` |
| **HẠ NHIỆT** | `z ≤ −2` |
| **TẮT** | `n_T = 0`, `n_P ≥ 3` |

### 4.4 Quan hệ giữa các sự kiện — 4 tầng bổ sung nhau

**L1 — Trùng ngôn ngữ (đã có).** `story_links` so bigram + lọc IDF. Giữ nguyên,
chỉ giới hạn vào các story trong lát cắt.

**L2 — Đi trước / đi sau (MỚI, giá trị cao nhất).** Với cặp `(A,B)` cùng hoạt
động, lấy chuỗi giờ 48 điểm, tính tương quan Pearson ở các độ trễ `L = 0..6`:

```
r(L) = corr( a[L..48], b[0..48−L] )
```

Báo cạnh `A → B (trễ L giờ)` khi `max_L r ≥ 0.6`, `L ≥ 1`, và cả hai có ≥5 bài.

⚠️ **Bắt buộc có chốt chặn thống kê.** ~95 story ⇒ ~4 465 cặp × 7 độ trễ. Ở quy
mô đó, tương quan giả xuất hiện *chắc chắn*. Phải có **kiểm định hoán vị**
(xáo khối chuỗi B 200 lần, chỉ giữ nếu r quan sát vượt phân vị 95) hoặc
**hiệu chỉnh FDR Benjamini–Hochberg ở mức 0.1**. Không có bước này thì L2 chỉ là
máy sinh quan hệ bịa.

⚠️ **Gọi đúng tên: "đi trước", KHÔNG phải "gây ra".** Granger causality là bản
chặt chẽ hơn, nhưng 48 điểm dữ liệu đếm bùng nổ thì Granger thiếu lực kiểm định
— kiểm định hoán vị trung thực hơn và rẻ hơn.

**L3 — Cùng bùng (co‑burst).** `z(A) ≥ 2` và `z(B) ≥ 2` trong cùng T, **và** có
cạnh L1. Đây là tín hiệu mạnh nhất của "cùng một mạch chuyện lớn" mà không cần
suy diễn nhân quả.

**L4 — AI diễn giải (đã có hạ tầng).** Đưa bảng biến động + cạnh L1/L2/L3 vào
`news_analyze_graph`. Điểm khác biệt so với hiện tại: AI được xem **cả chiều
biến động và độ trễ**, nên có thể giải thích *vì sao* lát cắt dịch chuyển, chứ
không chỉ mô tả bản đồ tĩnh. Vẫn bắt buộc `sanitize_map` đối chiếu id thật.

### 4.5 Nguyên tắc phân vai giữ nguyên: **máy đếm, AI diễn giải**

L1–L3 phải deterministic, kiểm thử được. AI chỉ chạm vào L4.

---

## 5. Bề mặt kỹ thuật

Dữ liệu **đã đủ**, không cần bảng mới (trừ cờ ngày thật ở Bẫy 2):
`articles(published_at, story_id, source_id)` + `stories(id, title)`.

- Rust: `cluster::slice_stories()`, `cluster::score_movement()`, `cluster::lead_lag()`
- REST: `GET /api/slice?hours=24` · `POST /api/slice/analyze`
- MCP: `news_slice`, `news_slice_analyze` (theo đúng quy ước `news_*`)
- UI: tab **Lát cắt** — biểu đồ slope (share T‑1 → T, tô màu theo nhãn) + bảng
  biến động + mũi tên độ trễ chồng lên graph sẵn có

Chi phí: ~95 story × 4 465 cặp × 7 độ trễ × 48 điểm ≈ 1,5 M phép nhân — không
đáng kể. Kiểm định hoán vị 200 lần đắt hơn 200×, vẫn dưới một giây.

---

## 6. Kế hoạch kiểm chứng (viết test TRƯỚC khi tin kết quả)

1. **Cửa sổ giống hệt nhau → không có biến động nào.** Test cơ bản nhất.
2. **Nhân đôi toàn bộ số đếm → vẫn không có biến động.** Chính là test chứng
   minh đã khử được Bẫy 1. Hiện tại code sẽ trượt test này.
3. **Chuỗi tổng hợp có độ trễ 3h đã biết → `lead_lag` phải tìm ra đúng L = 3.**
4. **Hai chuỗi ngẫu nhiên độc lập → kiểm định hoán vị phải LOẠI.** Test chống
   quan hệ giả.
5. **Ví dụ thật Aeon Mall (14 → 57):** sau chuẩn hoá phải ra "duy trì",
   không phải "bùng phát". Đây là ca kiểm chứng tốt nhất vì đáp án đã biết.

---

## 7. Giới hạn phải nói thẳng

- **48 điểm giờ là ngắn.** L2 chỉ mang tính gợi ý, kể cả khi qua kiểm định.
- **Chỉ phủ ~5% story** (95/1 729). Sự kiện một‑bài không bao giờ vào lát cắt —
  đó là chủ ý, nhưng nghĩa là tin độc quyền của một báo sẽ bị bỏ qua.
- **21,7% bài không có ngày thật** cho đến khi sửa Bẫy 2.
- **Bài đăng lại (syndication) thổi phồng độ lan:** 10 báo đăng lại cùng một bản
  tin hãng = 10 "nguồn" nhưng chỉ 1 tường thuật độc lập. Clustering gom phần lớn
  vào một story, nhưng `src_T` vẫn đếm 10.
- **48h tới, lát cắt sẽ méo** vì tập nguồn vừa nhảy từ 9 lên 68. Chỉ đáng tin từ
  2026‑07‑31 trở đi, hoặc phải bật lọc giao tập nguồn ngay từ đầu.

---

## 8. Thứ tự làm nếu triển khai

1. Cờ ngày thật (Bẫy 2) — thay đổi schema nhỏ, chặn nhiễu lớn nhất.
2. `slice_stories()` + lọc giao tập nguồn (Bẫy 1) + cổng độ lan (Bẫy 3).
3. Chấm điểm + phân loại, kèm test 1/2/5.
4. L2 lead‑lag + kiểm định hoán vị, kèm test 3/4.
5. REST + MCP.
6. UI tab Lát cắt.
7. L4 — nối vào AI.

Bước 1–3 đã tự nó có giá trị: chỉ riêng "sự kiện nào thật sự nóng lên sau khi trừ
trôi khối lượng" đã là thứ hiện tại app **không** trả lời đúng được.
