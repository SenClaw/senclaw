# Tài liệu đi cùng dự án

**Trạng thái:** đã cài đặt 2026-09-12. Chưa chạy trên daemon thật (xem cuối bài).
**Kế hoạch gốc:** [260912-1419](../plans/260912-1419-tai-lieu-hoa-thay-doi/README.md)
· **Nghiên cứu:** [research-260912-1419](../plans/reports/research-260912-1419-tai-lieu-hoa-thay-doi.md)

## Vì sao

Hỏi "cho tôi tài liệu về X" thì nhận một khối chữ trong chat rồi nó cuộn mất.
Sửa xong một tính năng thì không còn gì để người thứ hai theo được flow code.

Ba thứ trong repo đã ghi markdown, không thứ nào lấp được chỗ này:

- **Plan mode** ghi một file plan, nhưng đó là **dự định trước khi làm**, và tắt
  Plan mode là không còn gì.
- **Wiki** (`~/senclaw/wiki`) là kho tri thức **cá nhân**, dùng chung cho mọi dự
  án. Tài liệu về code của một repo phải đi cùng repo đó, để clone sang máy khác
  vẫn còn.
- **`checkpoint_explain`** đã diễn giải diff bằng LLM — nhưng chỉ vào chat.

## Đã thêm gì

| Thành phần | Ở đâu | Làm gì |
|---|---|---|
| Lõi lưu trữ | [`src/docs/`](../src/docs/mod.rs) | Ghi/đọc/liệt kê `.md` trong `<working_dir>/docs`, sinh chỉ mục, chặn đường dẫn |
| 4 MCP tool | [`src/mcp/docs_server.rs`](../src/mcp/docs_server.rs) | `doc_write`, `doc_read`, `doc_list`, `doc_index_rebuild` trên server `senclaw-docs` |
| Quy ước cho agent | [`skills/project-docs/SKILL.md`](../skills/project-docs/SKILL.md) | Viết gì, khi nào, phân biệt với wiki |
| Luật trong prompt | `project_docs_block` trong [`prompt.rs`](../src/zen_core/prompt.rs) | Nhắc mỗi lượt, **chỉ khi** dự án đã có `docs/` |
| Checkpoint → bản ghi | `checkpoint_document` trong [`checkpoints.rs`](../src/gateway/ui_server/checkpoints.rs) | `POST /api/chats/:jid/checkpoints/:id/document` |
| Nút bấm | `ChangesPanel.tsx` (web), `changes_tab.dart` (desktop) | "Ghi tài liệu" cạnh "Explain" |

## Bốn quyết định, và lý do

**Tài liệu vào repo, không vào wiki.** Gốc là `<working_dir>/docs`. Working dir
đọc từ **file state của workspace ở mỗi lần gọi**, không chụp lại lúc spawn: một
chat đổi workspace giữa phiên (`workspace_switch`) thì bản chụp cũ vẫn ghi thành
công vào thư mục cũ — sai mà không báo gì.

**Không tự động ghi mọi thay đổi.** Ghi theo yêu cầu, cộng một nút biến một
checkpoint thành bản ghi, cộng một luật trong prompt chỉ *đề nghị* khi thay đổi
tới hành vi / hợp đồng / quyết định. Tự động mọi lần sẽ làm repo ngập.

**Không tự `git commit`.** Wiki tự commit vào repo của chính nó. Đây là repo của
người dùng, nên chỉ ghi file rồi nói đường dẫn.

**Ngôn ngữ dò từ dự án, không phải một setting.** `DocsStore::language()` đọc
tiêu đề và câu mô tả của các tài liệu sẵn có; thấy dấu tiếng Việt thì tài liệu
mới viết tiếng Việt. Dự án chưa có tài liệu nào thì theo ngôn ngữ người dùng
đang viết.

## Những chỗ dễ làm sai

- **Chặn đường dẫn phải canonicalize **cả hai** phía.** Một symlink trong `docs/`
  trỏ ra ngoài là đường dẫn hoàn toàn bình thường, không có `..` nào — chỉ resolve
  mới bắt được. Nhưng canonicalize một phía thôi thì trên macOS một bên là
  `/var/...`, bên kia `/private/var/...`, và **mọi** lần ghi trông như thoát ra
  ngoài. `resolve_as_far_as_it_exists` chạy cho cả hai.
- **File chưa tồn tại thì `canonicalize` thất bại**, mà đó là trường hợp thường
  gặp nhất (tài liệu sắp ghi). Phải resolve phần sâu nhất **đang tồn tại** rồi
  nối phần còn lại theo chữ.
- **Không ghi đè khi chưa được yêu cầu.** `doc_write` từ chối path đã có trừ khi
  truyền `overwrite`. Tài liệu người dùng tự viết không phải thứ để đoán.
- **Chỉ mục chỉ ghi lại phần giữa hai dấu mốc.** Chữ người dùng viết ngoài khối
  đó còn nguyên; README chưa có mốc thì được thêm khối một lần, không phá gì.
- **Dự án chưa có `docs/` thì không tự tạo.** `rebuild_index` trả `None`, luật
  trong prompt không được chèn, và endpoint trả 400 có nêu lý do. Tạo cấu trúc
  trong repo người khác là quyết định của họ.
- **Bản ghi thay đổi tách số liệu khỏi suy luận.** Danh sách file lấy thẳng từ
  diff kèm trạng thái (thêm/xoá/sửa/đổi tên); phần diễn giải là LLM đọc diff,
  và tài liệu ghi rõ câu đó ngay trên đầu đoạn. Trộn hai thứ là mời người đọc
  tin cái đoán ngang cái đo được.
- **Hai bản ghi cùng chủ đề trong một ngày không đè nhau** — file thứ hai được
  đánh số.
- **Bốn tool phải đúng tên đã hứa.** Đổi tên hàm là đổi tên tool trong im lặng,
  và triệu chứng duy nhất là agent được dạy gọi `doc_write` rồi không tìm thấy
  tool giữa lượt. Có test ghim.

## Đã kiểm chứng

| Việc | Cách |
|---|---|
| Ghi/đọc/liệt kê, chỉ mục giữ chữ người viết, xoá file thì rời chỉ mục | 10 test trong `src/docs/` |
| Chặn `..`, đường dẫn tuyệt đối, không phải `.md`, symlink trỏ ra ngoài | test riêng cho từng dạng |
| Slug gập dấu tiếng Việt, không rỗng, không quá dài | 1 test |
| Working dir đọc lại mỗi lần gọi | 1 test đổi file state giữa hai lần đọc |
| 4 tool đúng tên | 1 test trên tool router |
| Luật prompt chỉ xuất hiện khi có `docs/`, và nêu đúng ngôn ngữ | 2 test |
| Biên dịch | `cargo test --lib` 2383 xanh, `cargo check --all-targets` sạch, `flutter analyze` sạch 2 app, `flutter test` 262 xanh, `npx tsc` sạch |

## Chưa kiểm chứng

- **Chưa chạy trên daemon thật.** Không có lượt nào gọi `doc_write` qua một
  agent sống; đường dây MCP chỉ được biên dịch và test ở mức router.
- **`checkpoint_document` chưa gọi thật** — nó cần một lệnh LLM, nên chưa có
  bản ghi nào sinh ra từ diff thật.
- **Nút trên hai client chưa bấm thử**, vì cả hai cần daemon mới.
