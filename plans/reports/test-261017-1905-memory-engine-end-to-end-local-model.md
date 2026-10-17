# Thử nghiệm memory mới end-to-end (model cục bộ)

Ngày 2026-10-17 · binary daemon `0.1.6 e92c23f` (đúng bản đã cài) · model `gemma-4-e2b-it-OptiQ-4bit` qua `sen-mlx 0.1.1` · daemon cô lập (HOME/SENCLAW_HOME/cwd tạm, cổng 28788/28789) · chat qua WS gateway như UI · cửa sổ reflection đặt 6 giây (mặc định 120 giây) để thử nhanh.

Không có embedding provider → chỉ đo keyword / graph / temporal; arm `facts_semantic` chưa được thử.

## Kết quả

| Kiểm tra | Kết quả |
|---|---|
| **Ghi**: nói tên, công ty, sở thích → fact | Đạt. 2 fact sau lần flush đầu ("Người dùng tên là Lan và làm việc ở công ty Orion", "Người dùng thích sử dụng React cho frontend") |
| **Đọc ở chat khác** của cùng agent (không chung lịch sử) | Đạt. Model trích nguyên khối `<memory_context>`: "[1] (fact) … tên là Lan và làm việc ở công ty Orion. [2] (fact) … thích sử dụng React" |
| **Ngày tương đối**: "hôm qua sửa xong lỗi đăng nhập" | Đạt. Fact có ngày `2026-10-16` (hôm nay 2026-10-17) |
| **Secret** `sk-proj-…` trong tin nhắn | Đạt ở kho fact: 0 dòng `mem_facts` chứa khoá. Fact chỉ nói "cung cấp khóa API của mình" (có entity `api_key`) |
| **Nhắc lại** cùng điều | Đạt một phần: `proofCount` ×2/×3 ở nhiều fact (bộ lọc "nói lại" và dedupe hoạt động) |
| **Đổi ý** (React → Vue → Svelte) | **Không đạt.** `superseded = 0` cả hai lần; fact cũ vẫn còn và vẫn được recall |
| **Quên** qua giao diện web | Đạt. Xoá đúng 1 fact, 8 → 7 |
| **Quên → fact cũ sống lại** | Chưa thử được (không có fact nào bị thay) |

## Vấn đề thấy được

1. **Đổi ý không kích hoạt trên đường thật.** Dòng log mới `replaces_claimed=0` cho thấy model **không nêu** `replaces` ở cả hai lượt, trong khi harness (7 cửa sổ dựng sẵn) đo được 15/18 lần nêu. Hai khác biệt có thể giải thích: cửa sổ thật chứa cả câu trả lời dài của assistant, và danh sách `KNOWN FACTS` chọn theo từ khoá từ cả cửa sổ. Chưa kiểm chứng — cần log số dòng `KNOWN FACTS` mỗi chunk và thử cửa sổ chỉ gồm lượt người dùng. Hậu quả hiện tại: sau lượt Svelte có đồng thời fact React, Vue và Svelte.
2. **Vòng lặp vọng lại.** Câu trả lời của assistant nhắc lại điều vừa được recall ("Tôi đã ghi nhớ…") rồi bị rút lại thành fact mới với câu chữ khác: "Tên của người nói là Lan…", "Assistant đã ghi nhớ thông tin của Lan…", "Lan là người dùng… trước đây có sở thích React". 3/8 fact sau 6 lượt là vọng lại; bộ lọc `restated` không bắt được vì fact gộp nhiều ý hoặc nói về hành động của assistant.
3. **Cửa sổ bị cắt giữa lượt** khi model sinh lâu hơn thời gian im (6 giây trong thử nghiệm): lượt người dùng và câu trả lời thành hai cửa sổ, nên có fact "Assistant đã ghi nhớ…" đứng riêng. Với mặc định 120 giây thì ít xảy ra trừ model rất chậm.
4. **Chi phí trên model nhỏ**: một lượt chat 45–150 giây (system prompt rất dài), mỗi lần trích xuất 15–30 giây; `sen-mlx` phục vụ tuần tự nên trích xuất chờ lượt chat.
5. **Khoá API còn nằm nguyên văn** trong `llm-sessions/*.json`, `llm_logs/*.log` của thư mục dữ liệu (lịch sử chat người dùng gõ) — có từ trước, không phải do engine fact; engine chỉ bảo đảm khoá không vào kho fact và không tới model trích xuất.

## Đã thêm trong lúc thử

- Dòng log retain nay có `replaces_claimed`, `superseded`, `failed_chunks` (trước chỉ có `stored/reinforced/folded`), và `RetainReport.replaces_claimed` — để phân biệt "model không nêu" với "retain từ chối".
- Route `GET /api/memory/facts/banks` và tab **Facts** trong trang Knowledge (web + desktop) — xem [docs/memory-engine.md](../../docs/memory-engine.md).

## Câu hỏi chưa giải quyết

- Có nên chỉ đưa lượt người dùng (hoặc cắt phần assistant lặp lại khối `<memory_context>`) vào cửa sổ trích xuất để chặn vòng lặp vọng lại?
- Đổi ý có cần một đường không dựa vào model nêu `replaces` (ví dụ so khớp chủ đề + cụm "đổi ý / không còn") cho model nhỏ?
- Đo lại trên `gemma-4-e4b` thật khi máy LM Studio `192.168.0.93` lên.
