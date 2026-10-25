# Nghiên cứu: đồng bộ kiến thức giữa hai SenClaw qua SLink

Ngày 2026-10-20. Chỉ nghiên cứu, chưa viết code. Mã nguồn tham chiếu: senclaw `main` = v0.2.1 (d5a2e70).

## 1. Kết luận ngắn

- **Mirror, đừng merge.** Mỗi bên nhận kiến thức của bên kia vào một kho riêng, chỉ đọc (`peer:<daemonId>`), có nhãn nguồn. Không trộn vào kho của mình. Hai chiều = hai mirror một chiều. Tránh toàn bộ bài toán xung đột, supersede, xoá, ID khác nhau.
- **Làm theo bậc, bắt đầu bằng truy vấn tại chỗ (federated recall)**, chưa sao chép gì: bên này hỏi bên kia "bạn biết gì về X" qua kênh SLink đã mã hoá. Rẻ, an toàn, đo được ngay.
- **Mặc định tắt, bật theo từng peer, từng loại.** Dữ liệu thật của bạn có PII (email khách hàng trong ghi chú) và lịch sử chat; đồng bộ nhầm là rò rỉ không rút lại được.
- Hôm nay **chưa có phương thức nào trong SLink mang kiến thức**: `handle()` chỉ có `SendMessage/GetTask/ListTasks/CancelTask` (`src/sen_link/server.rs:212`). Cần thêm phương thức mới trong cùng phong bì mã hoá.

## 2. Kiến thức nằm ở đâu (bằng chứng từ máy này)

| Kho | Dạng | Dữ liệu thật trên máy bạn | Hợp để đồng bộ? |
|---|---|---|---|
| Facts (`mem_facts`) | câu tự đủ nghĩa, SQLite | **20 fact**: 19 trong chat `schedule_24763f5d…`, 1 trong `main`; **0 fact toàn agent** (`origin=''`), 0 fact gõ tay, 0 vector | Có, nhưng hôm nay gần như rỗng |
| Ghi chú curated (`profiles/*/memory/*.md`) | markdown + frontmatter, chỉ mục `MEMORY.md` | 324 file: **92 nhật ký thô**, **126 `conversation-summary-*`**, ~106 ghi chú thật; 24 `MEMORY.md` | Chỉ ~106 ghi chú thật. Mẫu có email khách hàng |
| Wiki (`~/senclaw/wiki`) | trang markdown, git cục bộ | 33 trang; `git.rs` chỉ có `get_history`, không có remote/push/pull | Để sau |
| Đồ thị cognitive (`cog_*`) | node/edge + trọng số Hebbian | có bảng; đang được đề xuất gộp vào facts (docs/knowledge-cognitive-flow.md) | Không: dữ liệu dẫn xuất, không merge được rẻ |
| `USER.md`, `AGENTS.md`, SOUL | hồ sơ chủ máy | — | Không bao giờ (có phân tầng, quy tắc chỉ-đọc trong nhóm) |

Hệ quả: **giá trị thật nằm ở ghi chú curated và wiki, không phải facts.** Một kế hoạch chỉ sao chép facts với phạm vi an toàn mặc định sẽ chuyển **0 bản ghi** trên máy bạn.

## 3. Ràng buộc từ code (điều làm bài toán khó)

1. **Fact không có ID ổn định giữa máy.** `id` là UUID mỗi máy; danh tính nội dung là `content_hash = hash(text, origin)`, duy nhất theo `(bank, content_hash)` (`facts/store.rs:51`, `:421`). `origin` là JID chat cục bộ, vô nghĩa ở máy kia. `bank` = tên thư mục profile cục bộ.
2. **`insert()` không idempotent về ý nghĩa.** Gặp fact trùng hash thì `proof_count += 1` (`store.rs:430`). Đồng bộ lặp lại sẽ thổi phồng độ tin cậy. Cần đường nhập riêng không đếm.
3. **Supersede dùng ID cục bộ** (`superseded_by`), fact gõ tay không bao giờ bị thay (`store.rs:511`), xoá một fact tự khôi phục fact nó đã thay (trigger `mem_facts_restore`). Sao chép nguyên trạng sẽ vỡ các quan hệ này.
4. **Vector gắn với model.** `search_vector` lọc `model = ?` (`store.rs:768`); vector của máy kia không dùng được nếu khác model. Máy bạn hiện có 0 vector (chưa cấu hình embedding).
5. **Không có change feed.** `mem_facts` không có `updated_at`/số thứ tự/tombstone. Đồng bộ delta cần cột mới; đối chiếu theo tập hash thì không cần.
6. **Phạm vi chat.** `Scope::Chat` ẩn fact của chat khác (`store.rs:205`). Xuất khẩu phải tôn trọng tương đương, mặc định chỉ `origin=''`.
7. **Ghi chú:** `MemoryManager` chỉ quét `memory/*.md` **không đệ quy** (`manager.rs:265`); thả file mới vào đó là tự được lập chỉ mục, nhưng thư mục con thì không.
8. **Giới hạn kênh SLink** (đo ngày 20/10 trên cặp thật): RTT p50 5 ms, p95 18 ms; 30 cuộc gọi song song đều ổn; thân request tối đa 1 MiB (`listener.rs:32`), thực tế ~700 KB/tin (900 KB bị 413); trả lời tối đa 5 MiB (`seal.rs`). Lô cần ≤ ~500 KB.
9. **Đường đọc có sẵn chịu được peer chậm:** mỗi arm có timeout (mặc định 3 s) và lỗi chỉ mất hit của arm đó (`recall.rs:34`). Nhưng quy tắc "đường đọc không gọi model" vẫn giữ: bên kia chỉ được trả lời bằng tìm kiếm FTS/vector, không LLM.
10. **Không có thương lượng khả năng:** thẻ Agent Card chỉ mang `daemonId`, `pairable` (`a2a.rs:96`); không xử lý header `A2A-Extensions`. Peer v0.2.1 gặp phương thức lạ sẽ trả `-32601` — đó là tín hiệu "không hỗ trợ" sạch.

## 4. Các phương án

| # | Phương án | Ưu | Nhược | Đánh giá |
|---|---|---|---|---|
| A | Hỏi qua task (`link_send` "bạn biết gì về X?") | Làm được ngay, không code mới | Cần dispatcher bên kia (**đang tắt/không nhận việc**), tốn LLM, trả lời văn xuôi, chậm | Không phải đồng bộ |
| B | **Federated recall**: RPC `Recall` + tool `link_recall` + (tuỳ chọn) arm đọc | Không sao chép, không xung đột, nhất quán tức thì, kiểm soát theo từng truy vấn | Cần peer online; thêm độ trễ ở đường đọc (cần timeout + circuit breaker) | **Bước 1** |
| C | **Mirror** facts/ghi chú vào kho `peer:<id>` chỉ đọc, đối chiếu theo hash | Offline được, đọc nhanh, bỏ peer = xoá một kho, không đụng dữ liệu của mình | Cần cột/đường nhập mới, re-embed, quản lý bản cũ | **Bước 2–3** |
| D | Merge hai chiều vào cùng một kho | Trải nghiệm "một bộ nhớ" | ID, supersede, xoá, đồng hồ, PII, injection: khó nhất; sai là mất/ rò dữ liệu | Không làm |
| E | Wiki qua git bundle/fetch trên SLink | Có lịch sử | Khối lượng lớn, wiki chỉ 33 trang, chưa có remote | Để sau |

## 5. Thiết kế đề xuất (B rồi C)

**Khả năng và chính sách theo peer** (mặc định tắt):
- Thẻ Agent Card thêm `knowledge: {v:1, recall, mirror}`; peer cũ không có = không hỗ trợ.
- `Peer.share`: `facts` = off | agent-wide | các profile chọn; `notes` = off | các loại `project/reference/feedback` ở profile chọn; `projects` = danh sách project. `Peer.read`: `recall`, `mirror`. Tách khỏi `autoRun`: cho xem kiến thức ≠ cho chạy việc.
- Xem trước "sẽ chia sẻ gì" (đếm theo loại, mẫu 5 dòng) trước khi bật; ghi lại lần xuất cuối.

**Phương thức mới trong cùng phong bì mã hoá** (sau `handle()`):
- `knowledge/recall {query, k≤10, kinds}` → `[{text, kind, when, entities, source, score}]`, chỉ qua FTS/vector của bên trả lời.
- Mirror, kéo về (mỗi bên tự kéo của bên kia): `knowledge/manifest {kind, after?}` → `[{hash, mentionedAt}]`; `knowledge/fetch {hashes≤200}` → nội dung. Đối chiếu theo tập hash; thứ gì peer không còn liệt kê thì bên này xoá khỏi bản mirror (xoá lan truyền mà không cần tombstone).

**Lưu trữ phía nhận:**
- Facts: `bank = peer:<daemonId>`, `origin=''`, `source='peer:<id8>'`, nhập qua hàm mới `insert_mirrored` (không tăng `proof_count`, `mentioned_at` bị kẹp ≤ bây giờ, bỏ vector nếu khác model rồi backfill). Đọc qua cơ chế "bank thứ hai" đã có cho project (`ProjectFacts` arms) mở rộng cho `peer:*`; kết quả hiển thị kèm "từ <tên máy>".
- Ghi chú: file `memory/peer-<id8>-<slug>.md` với frontmatter `origin: peer`, chỉ đọc; nằm cùng thư mục để watcher tự lập chỉ mục.
- "Nhận làm của mình" là hành động tường minh của người dùng (thành fact `manual`/ghi chú thường); đó là con đường duy nhất để kiến thức của peer vào kho của mình.

**Lọc phía xuất:** bỏ nhật ký thô, `conversation-summary-*`, ghi chú `type: user`, fact có `origin != ''`, mọi thứ không qua `defense::scrub`; giới hạn độ dài và số lượng mỗi lô.

## 6. Rủi ro và cách giảm

1. **Rò PII/chat** (mẫu thật có email khách hàng): mặc định tắt, danh sách cho phép theo profile+loại, xem trước, lọc xuất như trên.
2. **Prompt injection qua kiến thức peer** (peer có thể là người khác, không chỉ máy của bạn): kho riêng, nhãn nguồn, `defang`, giới hạn độ dài/số lượng, không bao giờ ghim vào `MEMORY.md`/ngữ cảnh cố định, tắt theo peer. Coi như dữ liệu không tin cậy (giống nội dung web).
3. **Phồng `proof_count`, lệch thời gian:** đường nhập riêng; kẹp thời gian tương lai.
4. **Khoá cặp bị lộ:** hôm nay đã cho chạy việc nếu `autoRun`; thêm quyền đọc kiến thức làm tăng bề mặt, nên tách quyền và hiển thị rõ.
5. **Lệch phiên bản:** peer cũ trả `-32601`, UI nói "peer chưa hỗ trợ".
6. **Embedding khác model:** mirror chạy keyword trước, backfill sau.
7. **Peer offline lâu:** hiện "đồng bộ lần cuối", mirror có TTL, hết hạn thì ẩn khỏi recall.

## 7. Lộ trình (ước lượng thô, chưa kiểm chứng)

- **P0 (1–2 ngày):** khả năng trong Agent Card, chính sách theo peer, API/UI xem trước, nút bật theo loại.
- **P1 (2–3 ngày):** `knowledge/recall`, tool MCP `link_recall`, tuỳ chọn arm đọc với timeout 300 ms + circuit breaker. Tìm cả facts lẫn ghi chú curated — vì facts đang rỗng.
- **P2 (4–5 ngày):** mirror facts (manifest/fetch, `peer:*`, `insert_mirrored`, reconcile, re-embed, "nhận làm của mình").
- **P3 (3–4 ngày):** mirror ghi chú curated.
- Không làm: đồ thị, `USER.md`/SOUL, nhật ký, tóm tắt hội thoại, lịch sử chat. Wiki để sau.

## 8. Kế hoạch kiểm thử

- Đơn vị: nhập lặp N lần không đổi `proof_count`; xoá ở nguồn làm biến mất ở mirror; `origin` bị xoá khi nhập; lọc xuất loại đúng loại file; lô > giới hạn bị chia.
- Hội tụ: hai daemon cô lập, thêm/xoá ngẫu nhiên, kéo qua lại, kết quả mirror = tập nguồn.
- Đường dây: relay ghi nhận (như đã làm cho pairing) — không có chữ nào của fact/ghi chú trên dây.
- Thật: cặp 192.168.0.82 ↔ 192.168.0.93 sau khi bên kia nhận việc được (xem 9.5).

## 9. Câu hỏi còn mở (cần bạn quyết)

1. Hai máy đều của bạn, hay sau này có SenClaw của người khác? Quyết định mặc định tin cậy và mức lọc.
2. Profile nào ứng với profile nào (`main` ↔ `main`?) và loại ghi chú nào được chia sẻ?
3. Có bao giờ cho fact của một chat cụ thể rời máy (chọn tay từng chat) không?
4. Mirror chỉ đọc + "nhận làm của mình" có đủ, hay bạn cần hợp nhất hai chiều thật sự vào một kho?
5. Chặn hiện tại: bên 192.168.0.93 chưa nhận việc (thẻ 6, 7 vẫn ở `ready` sau >15 phút) dù dispatcher đã bật theo bạn. Cần xác minh trên máy đó trước khi kiểm thử đầu-cuối bất cứ thứ gì dựa trên task.
6. Chưa xác minh trong nghiên cứu này: arm đọc có mở rộng sang nhiều bank `peer:*` được không (hiện `RecallQuery::project` chỉ một bank); worker nào chạy khi `assignee` trống.
