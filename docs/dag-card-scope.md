# Thẻ DAG thuộc về chat nào

**Trạng thái:** đã sửa 2026-09-12. Chưa chạy trên daemon thật (xem cuối bài).

## Triệu chứng

Mở một session mới hoàn toàn ("thời tiết ngày mai", chưa gửi câu nào) thì thấy
ba thẻ DAG của một session khác, kèm đủ danh sách task và persona.

## Nguyên nhân

Cả web và desktop lọc thẻ theo `adminFolder`:

```ts
.filter(p => p.adminFolder === group.folder)
```

`adminFolder` **là profile agent, không phải cuộc hội thoại**. Session mới trên
web lấy folder từ profile được chọn (`ChatPage.handleStartChat`: folder =
`chosenAgent.folder`, mặc định `main`), nên **mọi** session tạo bằng cùng một
profile dùng chung một folder. Lọc theo folder vì thế đúng bằng "hiện mọi DAG mà
profile này từng chạy" — kể cả trong một chat vừa mở.

`DispatchParent` không có trường nào khác để phân biệt: nó chỉ có `admin_folder`.

## Đã sửa

**Daemon ghi lại chat nào tạo DAG.** `DispatchParent.chat_jid` (`Option<String>`,
`#[serde(default)]` nên file state cũ vẫn parse). `dispatch_mcp_config` chèn
`SENCLAW_CHAT_JID`, `McpDispatchServer` đọc ra, `DispatchServer::with_chat_jid`
mang theo, và mỗi parent tạo ra được đóng dấu.

**Chuỗi rỗng nghĩa là không có chat, không phải một chat tên rỗng.** Daemon tự
đăng ký một dispatch server cho người gọi nội bộ (watch probe) với jid rỗng — chỗ
này có ghi chú sẵn trong `lib.rs` rằng spec dùng chung không được mang jid của
một chat. `with_chat_jid` lọc rỗng thành `None`.

**Client lọc theo chat, và có đường dự phòng cho dữ liệu cũ.**
`ownsDispatchParent` ([web](../web/src/utils/dispatchOwnership.ts),
[desktop](../desktop_app/lib/features/chat/dispatch_ownership.dart)):

1. Có `chatJid` → so sánh trực tiếp.
2. Không có (parent tạo trước khi có trường này) → tìm trong **tool message của
   chính chat đó** xem có nhắc id parent không. Đầu ra của
   `DispatchCreateParentAndRun` chứa dòng `Parent task created: p-7`, và tool
   message vốn đã lưu theo từng chat.
3. Không chat nào nhận → **không hiện ở đâu cả**, chứ không hiện ở mọi nơi. Một
   thẻ nằm sai hội thoại tệ hơn một thẻ thiếu, và danh sách dispatch riêng vẫn
   còn nó.

## Những chỗ dễ làm sai

- **So khớp id phải có biên từ.** `p-1` nằm trong `p-12`, nên một `contains`
  đơn thuần gán DAG khác cho chat này. Cả hai client dùng regex có biên.
- **Chỉ dò trong tool message.** Người dùng gõ "p-7 sao rồi?" vào chat không
  biến chat đó thành chủ của DAG.
- **Thêm trường vào `DispatchParent` là sửa ~30 struct literal trong test.**
  `#[serde(default)]` lo phần file state cũ, không lo phần biên dịch.
- **Đừng đổi `admin_folder`.** Nó vẫn là thứ tuần tự hoá DAG theo từng admin
  (`activate_next_queued`, `has_active`); chỉ việc *hiển thị* mới cần chat jid.

## Đã kiểm chứng

| Việc | Cách |
|---|---|
| Parent mới mang đúng chat jid, vẫn giữ `admin_folder` | 1 test đọc lại file state |
| jid rỗng thành `None` | 1 test |
| File state cũ (không có trường) vẫn parse | 1 test trên JSON bản cũ |
| Bốn nhánh của luật thuộc-về, kể cả `p-1` vs `p-12` | 5 widget test desktop; **cả 5 đỏ** khi đưa lối lọc theo folder trở lại |
| Biên dịch | `cargo test --lib` 2386 xanh, `flutter analyze` sạch, `flutter test` 267 xanh, `npx tsc` sạch, `npm run build` xong |

## Chưa kiểm chứng

- **Chưa chạy trên daemon thật.** Chưa có DAG nào được tạo qua một agent sống
  để xem `chatJid` chảy hết đường dây.
- **Đường dự phòng cho dữ liệu cũ chưa thử trên state file thật của người dùng**,
  chỉ trên dữ liệu dựng trong test.
