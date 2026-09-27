# Hợp đồng API của daemon

`docs/openapi-daemon.yaml` mô tả **mọi route** daemon phục vụ. Daemon cũng trả
chính tài liệu đó tại `GET /api/openapi.json`, nên client có thể tự đối chiếu
với bản daemon đang chạy thay vì tin vào một file đã cũ.

## Nó được sinh ra, không viết tay

Bốn SDK, ba client và một extension đều tự viết model tay cho cùng những route
này. Một spec viết tay bên cạnh hơn 350 route sẽ lệch ngay từ commit đầu — nên
spec này **đọc thẳng từ mã nguồn router** (`include_str!` vào
[`openapi.rs`](../src/gateway/ui_server/openapi.rs)): cùng đường dẫn, cùng
method mà axum đăng ký. Route mới xuất hiện; route bị xoá biến mất. Không có gì
phải giữ đồng bộ bằng tay.

Ba nguồn: `gateway/ui_server/core.rs` (gồm cả sub-router auth nó merge),
`kanban/api.rs` (nest tại `/api/kanban`), `sandbox/api.rs` (nest tại
`/api/sandbox`).

## Nó hứa gì và không hứa gì

| | |
|---|---|
| Chính xác | đường dẫn, method, tham số đường dẫn, tag, tên handler |
| **Không** mô tả | hình dạng body |

Phần lớn handler trả `Json<serde_json::Value>`, nên body ghi là `object` và
operation mang cờ **`x-untyped: true`**. Con số ở `x-untyped-operations` trong
chính spec là danh sách việc cần siết dần — đừng đọc nó như "API không có
kiểu", mà như "kiểu chưa được khai báo ở lớp này".

`:name` của axum được đổi thành `{name}` của OpenAPI. Vài template chồng nhau
(`/api/oauth/{provider}/start` và `/api/oauth/accounts/{id}`); lúc chạy không
mâu thuẫn vì axum khớp đoạn chữ trước đoạn bắt biến — client sinh ra nên chọn
template "chữ" nhiều hơn vì cùng lý do.

Security scheme khai báo cả ba cách daemon nhận token (bearer,
`X-SenClaw-Token`, cookie `senclaw_token`). Hai đường mở
(`/api/auth/status`, `/api/auth/login`) mang `security: []` — client phải hỏi
được "có cần token không" trước khi có token. Việc token **có bị đòi hay không**
là `SENCLAW_AUTH_MODE`, xem [remote-access-security.md](remote-access-security.md).

## Khi thêm route

Không phải làm gì cả — trừ việc làm mới bản đã commit:

```bash
SENCLAW_WRITE_OPENAPI=1 cargo test --lib openapi::tests::committed
```

Test `committed_spec_matches_the_routers` sẽ đỏ nếu `docs/openapi-daemon.yaml`
cũ hơn router, nên bản trong repo không thể lặng lẽ lệch.

## Kiểm

```bash
npx @redocly/cli lint docs/openapi-daemon.yaml
```

Sạch lỗi. Còn cảnh báo `tag-description` (tag sinh từ đoạn đường dẫn nên chưa
có mô tả người viết) và `no-ambiguous-paths` (đã giải thích ở trên).

Spec của **ClawHub registry** là tài liệu khác: [openapi-hub.yaml](openapi-hub.yaml).
