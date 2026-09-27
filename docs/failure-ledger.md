# Sổ sự cố — ghi lại cái hỏng, chưa học gì từ nó

> **Trạng thái: ĐÃ CÀI** · 2026-09-21 · GĐ 1 của
> [error-learning-research.md](error-learning-research.md).
> Code: [`src/failures/`](../src/failures/mod.rs),
> [`src/db/failures.rs`](../src/db/failures.rs),
> REST [`src/gateway/ui_server/failures.rs`](../src/gateway/ui_server/failures.rs).

## Vì sao có nó

Engine **phát hiện lỗi tool rất tốt**: đếm số lượt liên tiếp mà mọi tool vừa
chạy đều hỏng, nhắc model một lần, rồi dừng lượt và báo lỗi thật thay vì bịa ra
một câu trả lời thành công. Cái nó chưa từng làm là **giữ lại** bất cứ thứ gì —
ngày mai đúng tool ấy hỏng đúng kiểu ấy và hệ thống không khôn hơn chút nào.

Sổ sự cố đóng **nửa đo đạc** của khoảng trống đó, và chỉ nửa đó. Nó ghi, không
học. Lý do ghi trước khi học nằm ở §7 của tài liệu nghiên cứu: một kho bài học
chỉ đáng xây nếu lỗi thật sự lặp lại, và bảng này là cách duy nhất biết điều đó
trên dữ liệu thật thay vì phỏng đoán.

## Nó ghi gì

Một **episode** = một chuỗi lần hỏng liên tiếp của *cùng một tool* trong *cùng
một chat*, kèm kết cục.

| Cột | Ý nghĩa |
|---|---|
| `tool_name`, `chat_jid`, `agent_id` | ai hỏng, ở đâu |
| `args_shape` | JSON `tên tham số → kiểu`, **không bao giờ là giá trị** |
| `first_error` / `last_error` | nguyên văn, cắt ở 600 byte |
| `error_class` | dạng chuẩn hoá của `first_error` để đếm lặp bằng `GROUP BY` |
| `streak` | số lần hỏng liên tiếp trong episode |
| `status` | `open` / `resolved` / `gave_up` |
| `fix_source` | `model` hay `user` — ai là người sửa được |
| `loop_stopped` | bộ chống vòng lặp của engine đã cứng dừng trên tool này |
| `turns_ended` | số lượt agent kết thúc mà tool vẫn chưa chạy được |

Đường đi: `EngineEvent` → [`failures::record`](../src/failures/mod.rs) (cùng
seam với `trajectory::record` trong
[`engine.rs`](../src/agent/agent_pool/engine.rs)) → kênh mpsc → một worker ghi
tuần tự.

**Quy tắc kết cục**, tất định, không LLM:

- tool chạy được sau đó → `resolved`; `fix_source = user` nếu đã có tin nhắn
  người dùng xen vào sau lần hỏng đầu, ngược lại `model`;
- sống qua **hai** lượt agent kết thúc mà vẫn chưa chạy được → `gave_up`. Một
  lượt là lượt đang hỏng; lượt thứ hai là chỗ một câu đính chính của người dùng
  sẽ rơi vào. Xa hơn nữa thì gán công cho lần hỏng đó là đoán mò.

## Nó *không* làm gì

1. **Không học.** Không có bài học nào được chưng cất, không có gì từ bảng này
   quay lại prompt. Nhịp 2–4 của nghiên cứu vẫn chưa được viết, và **cổng để
   viết là số liệu từ chính bảng này** — xem "Cổng quyết định".
2. **Không giữ giá trị tham số.** Chỉ `tên → kiểu`. Giá trị là đường dẫn, câu
   truy vấn, tên khách hàng; bảng này bền hơn cả cuộc trò chuyện sinh ra nó.
   `string:empty` được tách khỏi `string` vì "truyền `host` rỗng" và "không
   truyền `host`" là hai lỗi khác nhau.
3. **Không ghi việc người dùng từ chối quyền.** Đường từ chối trong
   [`run_tools.rs`](../src/zen_core/run_tools.rs) không phát
   `ToolExecutionError` — người nói không thì không phải tool hỏng.

## Đọc nó

```bash
curl -s "$UI/api/failures?limit=50" | jq
curl -s "$UI/api/failures?chatJid=web:main&status=gave_up" | jq
curl -s "$UI/api/failures/summary?days=30" | jq
```

`summary` trả về đúng ba con số quyết định:

| Trường | Trả lời câu hỏi | Ngưỡng |
|---|---|---|
| `repeatRate` | bao nhiêu % episode trùng `(tool, error_class)` với một episode khác | **< ~15% ⇒ bỏ tính năng học**, sửa mô tả tool rẻ hơn nhiều |
| `byStatus` | bao nhiêu lỗi rốt cuộc được sửa, bao nhiêu bỏ cuộc | — |
| `byFixSource` + `modelFixedAfterNudge` | model tự sửa (⇒ cú nhắc sẵn có đã đủ) so với người sửa (⇒ chỉ bài học nhớ được mới bắt được) | con số sắc nhất |

`topFailures` xếp theo `(tool, error_class)` — nếu một dòng chiếm phần lớn bảng
thì việc cần làm là sửa mô tả tool đó, không phải xây hệ học.

## Quy tắc cho Claude

- **Đừng nhầm bảng này với `tool_executions`.** Bảng kia là bộ đệm phát lại của
  chat và bị FIFO-trim theo hạn mức tin nhắn — tức là **những lần hỏng cũ nhất
  bị xoá trước**, đúng thứ chứng minh một lỗi lặp lại. Bảng này bền, ở mức
  episode, và giới hạn bằng số dòng (20 000) chứ không theo chat.
- **`open` không phải là lỗi.** Nó là trạng thái nghỉ của một episode chưa kết
  thúc. Tương tự, một Space App `session` đang dừng là trạng thái nghỉ chứ
  không phải hỏng — một hệ học ngây thơ sẽ ghi "đừng gọi `space_app_start`".
- **Tập `OPEN` trong bộ nhớ phải được nạp lại lúc khởi động.** Nó là bộ lọc cho
  đường nóng (mọi lần gọi tool *thành công*); không nạp thì sau một lần restart
  các dòng `open` không bao giờ đóng được nữa.
- **Sáu giờ là ranh giới episode.** Một lần hỏng cách lần trước quá xa là
  episode mới; dán chúng lại cho ra một `streak` vô nghĩa và làm hỏng đúng con
  số `repeatRate` đang đo.
- **`details` của `SessionError{tool_error_loop}` là dữ liệu có cấu trúc**
  (`toolSig`, `streak`), không phải câu chữ để parse ngược từ `message`.
- **Tên trường trong `Query<T>` chính là tên trên dây.** `chatJid` cần
  `rename`, snake_case chỉ là `alias` — cùng cái bẫy đã làm `/api/watches` im
  lặng trả 400.

## Cổng quyết định

Sau 1–2 tuần dữ liệu thật:

- `repeatRate` ≥ ~15% **và** phần lớn lỗi quy được về **tiền đề sai** (thiếu
  tham số, sai thứ tự) chứ không phải **môi trường** (timeout, 5xx) ⇒ làm tiếp
  GĐ 2 (bộ phân loại quy trách nhiệm, fail-closed).
- Ngược lại ⇒ **dừng**. Sửa mô tả tool theo `topFailures`.

Trong cả hai trường hợp, [evals loop](evals.md) phải chạy được **trước** khi
bất cứ thứ gì bắt đầu ghi vào prompt: bài học đổi system prompt là đổi *mọi*
case, kể cả case không liên quan.
