# Nghiên cứu: Elephant-Goldfish Model (google/elephant-goldfish-model)

**Ngày:** 2026-10-27 · **Nguồn:** https://github.com/google/elephant-goldfish-model, commit `b3a3595` (2026-10-26), clone nông, chỉ đọc.
**Mục đích:** lấy thiết kế cho luồng spec của phiên Code trong SenClaw (brainstorm → draft → review 4 hướng: cảnh, kiểm, hiểu, phản biện).

## 1. EGM là gì

Plugin (skills + persona prompts + rubric markdown, không có code chạy) cho Antigravity / Claude Code / Codex. Thực hành
"Spec-Driven Development" của Dave Rensin:

| Vai | Ngữ cảnh | Việc |
|---|---|---|
| **Elephant** | toàn bộ dự án + lịch sử trò chuyện | thăm dò, viết spec |
| **Goldfish** | CHỈ tài liệu + file nguồn được nhắc tới, không có hội thoại | tìm giả định ngầm, chỗ mơ hồ, rủi ro |
| **Người** | mục tiêu, ràng buộc | quyết trade-off, **duyệt** spec trước khi code |

Lý do (docs/designs/architecture.md): một luồng vừa phỏng vấn, vừa thiết kế, vừa code thì tự xác nhận giả định sai của
chính nó ("hallucination cascade"); agent nhảy vào code ngay ("weeds"); dán spec dài vào chat làm hỏng ngữ cảnh; spec viết
xong rồi mục ("drift").

## 2. Luồng và file

| Lệnh | Làm gì | Ra |
|---|---|---|
| `/egm-brainstorm <ý tưởng>` | hỏi mở **từng câu một** về gốc vấn đề, hệ thống dùng nó, bất biến, non-goals → chốt "giả thuyết neo" 2-3 câu, người đồng ý → 6 goldfish "bắn hạ" (YAGNI, chi phí, UX, migration, observability, nợ kỹ thuật) | `concepts_brief.md` |
| `/egm-draft` | đọc brief, viết **từng phần**, dừng hỏi ý kiến sau mỗi phần; tự soát theo zoom-out standard | `docs/designs/<feature>.md` (nằm trong repo, commit cùng code) |
| `/egm-review <file>` | 5 bước goldfish, có cổng dừng giữa các bước | `*_report.md` từng bước + `consolidated_review_report.md` |
| `/egm-resolve` | Elephant xử lý từng finding | sửa spec |
| `/egm-drift-report` | so spec đã commit với code hiện tại (thiếu tính năng, model thiếu field, đường tắt hardcode, lệch kiến trúc) | `{date}_DRIFT_REPORT.md` |

Template spec (`skills/egm_draft/references/design_doc.md`): Overview, The Problem, Goals, Non-Goals, **Open Questions**
(giữ qua các phiên "để phiên sau không hỏi lại"), CUJs (Given/When/Then), Technical Plan, Alternatives Considered,
Non-Functional Requirements, rồi sau mốc **`## Detailed Implementation Plan`**: danh sách file kèm `(Create | Modify)` +
lý do, Verification Plan (build/test/lint cụ thể). Trường `Status: Draft | Proposed | In Review | Approved`.

**Ranh giới ngữ nghĩa:** phần trên `## Detailed Implementation Plan` = FeatureRange (kiến trúc), phần dưới = DetailedRange
(thực thi). Mỗi reviewer đọc đúng phần của mình; không cần cấu hình dòng.

## 3. Review: 4 lăng kính chính (khớp đúng "cảnh, kiểm, hiểu, phản biện")

| Của người dùng | Persona EGM | Đọc phần | Rubric | Hỏi gì |
|---|---|---|---|---|
| **Cảnh** (toàn cảnh) | `zoom_out` – Skeptical Architect | cả tài liệu | zoom-out-standard | đủ ngữ cảnh để biết mục tiêu? giải gốc hay vá triệu chứng? YAGNI, tối ưu sớm, phát minh lại thứ repo đã có (**phải tìm trong code**), sa vào chi tiết, đặt sai tầng |
| **Kiểm** (kiểm chứng) | `implementation_readiness` + `codebase_details` | DetailedRange | implementation-readiness + re-implementation | "Zero-Decision Rule": người code không phải tự quyết gì; đủ file + Create/Modify; test cụ thể; mỗi phase build được. Mọi file/hàm/endpoint nói là **đang có** phải có thật; mâu thuẫn với code = Blocker. File **sẽ tạo** không phải lỗi |
| **Hiểu** | `comprehension` – Naive Reader | cả hai phần | tự đứng được | thuật ngữ chưa định nghĩa, điều kiện tiên quyết ngầm, "hệ thống tự lo" mà không nói cách nào |
| **Phản biện** | `design_critic` – Hostile Critic | FeatureRange | feature-spec + Heilmeier | "Alternatives Rule" (phải có phương án bị loại kèm lý do), giả định không chứng minh, điểm hỏng đơn, giới hạn tải, bảo mật, rủi ro chưa có biện pháp |

(Thêm: `structural`, `style` cho bố cục/văn phong; bước 8 chạy lại Zoom Out để cắt phần "phòng thủ thừa" mà các bước trước
thêm vào.)

**Định dạng finding:** Severity `Blocker | Major | Nit` (🔴🟡🟢) · Affected scope (mục / dòng) · Mô tả · Lý do + đề xuất.
Synthesizer gộp, khử trùng, xếp Blocker → Major → Nit, ghi ra file — **không đổ báo cáo dài vào chat**.

**Resolve:** mỗi finding được Elephant xếp `PROPOSE FIX` (sửa cụ thể) / `CONTEST` (phải dẫn chứng mà reviewer đọc được) /
`ESCALATE` (chỉ người biết) / `DECLINE` (Nit). Lỗ hổng "hiểu" **không được cãi** (người đọc mới không hiểu là đúng nghĩa
lỗi). Vòng lặp dừng khi 0 Blocker hoặc người dừng.

## 4. Luật thực thi đáng chép (resources/execution_instructions.md)

- **Chỉ thiết kế:** trong luồng thiết kế cấm ghi file nguồn; code minh hoạ chỉ trong khối markdown.
- **Cổng đồng thuận:** không sang bước sau khi người chưa "go-ahead".
- **Một câu hỏi mỗi lượt.**
- **Phát hiện mâu thuẫn** giữa yêu cầu mới và mục tiêu cũ → không ghi file trong lượt đó, chỉ nêu mâu thuẫn và hỏi.
- Không "tóm tắt công việc / checklist tuân thủ" ở cuối trả lời.

## 5. EGM KHÔNG có (SenClaw phải tự làm)

- **Không có cổng cứng.** "Chỉ thiết kế" và "người duyệt" là lời dặn trong prompt + PR review của người. Không gì chặn
  agent sửa code khi spec chưa duyệt. → Yêu cầu "đảm bảo thiết kế trước khi code" cần cổng thật ở tầng tool.
- **Không có cơ chế phiên sau đọc spec.** Chỉ dựa vào spec nằm trong repo + mục Open Questions. → Yêu cầu "các phiên xem
  spec trước khi làm tiếp" cần khối spec trong prompt mỗi lượt + kiểm "đã Read spec" trước khi sửa.
- **Goldfish cần harness tạo subagent mới.** Không tạo được thì EGM dừng review. Ở SenClaw daemon tự gọi model với ngữ
  cảnh trống — không phụ thuộc model chính có biết gọi `Task` hay không (quan trọng với gemma e4b).
- **Không có trạng thái duyệt máy đọc được.** `Status:` là chữ trong file, agent sửa được. → Duyệt phải lưu ngoài chỗ agent
  ghi được, gắn với hash nội dung.

## 6. Đề xuất mang sang SenClaw

1. Giữ 3 lệnh `/brainstorm`, `/draft`, `/review` + `/approve` (chỉ người gõ) — tên ngắn, không tiền tố.
2. Spec nằm trong repo `docs/designs/<slug>.md` (theo EGM, commit cùng code, máy khác vẫn thấy); brief cùng thư mục.
3. Template rút gọn của EGM, giữ mốc `## Detailed Implementation Plan`, `## Open Questions`, `(Create | Modify)`.
4. Review = 4 lăng kính goldfish chạy bằng lệnh gọi model **ngữ cảnh mới** do daemon điều khiển, đưa kèm "gói bằng chứng"
   (file được nhắc tới, outline repo, lệnh dự án), cộng **kiểm tra máy** cho "Kiểm" (đường dẫn/hàm có thật, đủ mục, có
   Create/Modify, có lệnh test) — model nhỏ không cần tự tìm.
5. Severity Blocker/Major/Nit; `/approve` từ chối khi còn Blocker hoặc review cũ hơn nội dung hiện tại (trừ khi người ép).
6. Báo cáo review ghi ra file, trong chat chỉ tóm tắt số lượng + Blocker.
7. Resolve theo 4 nhãn của EGM, lỗ hổng "Hiểu" không được cãi.
8. Drift: để sau (EGM coi là nghi thức hàng tuần) — ghi là việc tiếp theo, không làm trong đợt này.

## Câu hỏi chưa giải

- Có bật cổng cứng mặc định cho mọi phiên code không, hay chỉ khi người bật? (EGM khuyên "sửa lỗi nhỏ rõ ràng thì làm
  bình thường".)
- Có cần 6 lăng kính brainstorm như EGM, hay brainstorm chỉ hỏi-đáp + ghi brief (rẻ hơn cho model local)?
