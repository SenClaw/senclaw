# Sơ đồ mermaid trong chat

Một khối mermaid do model viết ra giờ **vẽ thành sơ đồ** trong bong bóng chat,
thay vì hiện nguyên mã — dạng duy nhất mà sơ đồ vô dụng.

| Client | Cách vẽ | Trạng thái |
|---|---|---|
| Web | `mermaid` npm, import động | ✅ đã kiểm chứng trên daemon thật |
| Desktop | webview nhỏ + `mermaid.min.js` đóng gói sẵn, phục vụ qua `InAppLocalhostServer` | ✅ đã thấy vẽ trong app thật |
| Mobile | — | chưa làm |

## Nhận cả fence không nhãn — đây là điểm cốt yếu

Model viết fence theo **hai** kiểu. Khi được yêu cầu vẽ sơ đồ nó thường ra
```` ```mermaid ````, nhưng cũng thường xuyên ra fence **trống** mà dòng đầu là
`flowchart TD`. Bản đầu chỉ tin vào nhãn, nên đúng ca thứ hai vẫn hiện mã thô.

Giờ một fence **không nhãn** được đối chiếu dòng đầu (bỏ qua dòng trống và
directive `%%{init: …}%%` của mermaid) với danh sách từ khoá sơ đồ của chính
mermaid: `flowchart`, `graph`, `sequenceDiagram`, `classDiagram`,
`stateDiagram`, `erDiagram`, `gantt`, `pie`, `mindmap`, `timeline`, … Từ khoá
phải **mở đầu** khối, điều không ngôn ngữ nào khác làm.

Phép đoán này cố ý hẹp:

- **Nhãn tường minh là lời của người viết và thắng mọi phép đoán.**
  ```` ```python ```` vẫn là Python dù dòng đầu có chữ `flowchart`;
  ```` ```mermaidish ```` không phải mermaid.
- **Chỉ khối nhiều dòng** mới có thể là sơ đồ — trong `react-markdown`, code
  inline và fence không nhãn đến cùng một hình dạng, nên một chữ `graph` giữa
  câu văn không bị biến thành sơ đồ.
- Một fence code bình thường *nhắc tới* đồ thị vẫn là code.

Nguồn dùng chung: [`web/src/utils/mermaidFence.ts`](../web/src/utils/mermaidFence.ts)
và `mermaidSource` trong
[`desktop_app/lib/widgets/mermaid_diagram.dart`](../desktop_app/lib/widgets/mermaid_diagram.dart).

## Sửa nhẹ cú pháp khi model viết sai — và chốt an toàn

Model viết sai cú pháp mermaid thường xuyên, và hai lỗi chiếm gần hết số ca
thật. Cả hai đều là "viết thứ trông hợp lý" chứ không phải thứ mermaid nhận:

- **Dấu ngoặc hoặc dấu phẩy trong nhãn mũi nối.** `-->|Tải (PNG, MP3)|` là lỗi
  parse — mermaid đọc `(` là mở hình khối, nên cần bọc nhãn trong ngoặc kép.
  Đây là lỗi **thật sự** làm sơ đồ không vẽ được.
- **`\n` dạng chữ** viết ở chỗ mermaid cần `<br/>`.

Phép sửa chỉ chạy **sau khi nguồn nguyên bản parse thất bại**, nên sơ đồ hợp lệ
không bao giờ bị viết lại. Nếu sửa xong parse được thì vẽ, kèm một dòng ghi chú
nhỏ: *"Sơ đồ đã được chỉnh nhẹ cú pháp để vẽ được."* — một sơ đồ bị chỉnh lặng lẽ
mà khác với văn bản ngay trên nó thì không ai giải thích được.

Nếu sửa rồi vẫn không parse, thứ hiện ra là **lỗi gốc**, không phải lỗi của bản
đã sửa: bản đã sửa không phải thứ tác giả viết, nên lỗi của nó sẽ chỉ người đọc
đi soi sai chỗ.

Cố ý **không** làm bộ sửa tổng quát. Nó không thể biết một sơ đồ hỏng đáng lẽ
phải là gì, và một phép đoán sai mà *parse được* còn tệ hơn một thông báo lỗi.

Vì `securityLevel: 'strict'` lọc thẻ HTML, `<br/>` bị bỏ — chữ vẫn còn đủ, chỉ
nằm trên một dòng thay vì hai. Đó là đánh đổi có chủ ý: giữ strict với dữ liệu
không tin cậy, mất ngắt dòng.

Hai bản: [`web/src/utils/mermaidFence.ts`](../web/src/utils/mermaidFence.ts)
`repairMermaid` và bản JS trong `desktop_app/assets/mermaid/index.html`. Hai
client không chia sẻ hàm được, nên sửa một bên thì phải sửa bên kia.

## Ba tính chất cả hai client phải giữ

1. **Chịu được streaming.** Câu trả lời về theo từng token, nên fence bị đọc lại
   khi thân còn viết dở. `mermaid.parse` ném lỗi với đồ thị chưa hoàn chỉnh —
   đó chính là tín hiệu: cứ hiện mã nguồn cho tới khi văn bản đủ, rồi mới đổi
   sang sơ đồ. Không nhấp nháy giữa hai sơ đồ vì chỉ lần render **thành công**
   mới thay nội dung.
2. **Sơ đồ lỗi vẫn phải hiện mã nguồn**, kèm lý do. Model sai cú pháp đủ thường
   xuyên để một ô trống, hoặc một thông báo lỗi làm mất văn bản, là mất đúng thứ
   người dùng muốn đọc.
3. **Nguồn là dữ liệu không tin cậy** — nó do model viết, mà model bị dẫn dắt
   bởi thứ nó vừa đọc. `securityLevel: 'strict'` tắt nhãn HTML thô và directive
   `click`, nên một sơ đồ không thể chèn markup hay gắn script vào nhãn.

## Web

`mermaid` import động, nên nó nằm ở chunk riêng (~155 KB gzip) và một cuộc chat
không có sơ đồ **không tải parser**. Hai chỗ dùng:
[`MessageBubble.tsx`](../web/src/components/MessageBubble.tsx) (bong bóng chat)
và [`MarkdownBody.tsx`](../web/src/components/shared/MarkdownBody.tsx) (wiki,
kết quả task, preview).

Override `pre` bỏ hộp code mà `react-markdown` bọc quanh mọi fence — cho mermaid
và cho cả các fence widget, vốn có sẵn cùng vấn đề.

## Desktop

Không có renderer mermaid viết bằng Dart, nên app **nhúng renderer thật** vào
một webview nhỏ trên bản `mermaid.min.js` đóng gói trong app: không cần daemon,
không cần mạng.

Hai điều widget này tồn tại để làm đúng:

- **Chiều cao phải bằng chiều cao sơ đồ.** Webview không có kích thước nội tại,
  nên hộp cố định sẽ cắt mất flowchart dài hoặc chừa khoảng trống dưới sơ đồ
  ngắn. Trang tự đo SVG của nó và báo về qua bridge; con số đó là chiều cao của
  widget. Có trần 1400px để một đồ thị chạy loạn không chiếm cả cuộc hội thoại.
- **Sơ đồ không parse được vẫn hiện mã nguồn.** Lúc streaming fence còn dở và
  mermaid từ chối là đúng; từ đây nhìn vào, chuyện đó không phân biệt được với
  model viết sai cú pháp. Cả hai đều hiện mã — không bao giờ tệ hơn văn bản thô
  mà người dùng thấy trước khi có tính năng này.

Linux không nhúng được webview trong app này, nên ở đó chỉ hiện mã nguồn **và
nói rõ lý do** — không lặng lẽ xuống cấp.

Asset: `desktop_app/assets/mermaid/{index.html,mermaid.min.js}` (5.4 MB). Đóng
gói thay vì tải về vì một sơ đồ phải vẽ được khi offline.

### Hai cái bẫy đã mất thời gian, ghi lại để không lặp

**`initialFile` không nạp trang trên macOS.** Không có lỗi nào được báo, không
có `onReceivedError`, view chỉ nằm trắng mãi. Trang giờ được phục vụ qua
`InAppLocalhostServer` — cơ chế riêng của thư viện cho nội dung local, bind
127.0.0.1, và làm `<script src="mermaid.min.js">` tương đối phân giải đúng như
trên web. Một server dùng chung cho mọi sơ đồ, cổng 19765 (ngoài mọi dải mà
daemon và Space App dùng).

**Webview cao ~0 thì JavaScript không chạy.** Bản đầu để view cao 1px cho tới
khi trang báo chiều cao — nhưng WebKit không layout view 1px nên JS không chạy,
nên nó không bao giờ báo được. Khoá chết do chính mình tạo ra, và biểu hiện là
mỗi sơ đồ thành một **vạch mảnh** phía trên mã nguồn của nó. View giờ có chiều
cao tạm 180px từ đầu, và chỉ khi trang **không trả lời trong 12s** thì Dart mới
tự hiện mã nguồn.

**Đo ngay sau `innerHTML` cho ra chiều cao gần 0.** SVG chưa layout theo bề rộng
đã co, web font chưa xong. Phép đo giờ lùi hai khung hình và đo lại khi
`document.fonts.ready`, cộng một lần nữa khi cửa sổ đổi kích thước — vì sơ đồ co
theo bề rộng nên đổi kích thước làm số đo cũ thành sai.

## Đã kiểm chứng những gì

**Web** — trên daemon thật, với câu trả lời thật của model:

- fence có nhãn `mermaid` → vẽ sơ đồ;
- fence **không nhãn** mở đầu `flowchart TD` → vẽ sơ đồ (đúng ca gặp thật);
- mermaid sai cú pháp → hiện mã nguồn kèm lỗi parse;
- đổi theme sáng/tối → vẽ lại theo bảng màu mới.

**Desktop** — đã thấy vẽ trong app thật, cộng kiểm headless trang render:

- sơ đồ căn giữa trong khung chat (lề hai bên bằng nhau);
- **sơ đồ lỗi thật của người dùng** (ngoặc trong nhãn mũi nối) → sửa nhẹ rồi vẽ
  được, 10 node, nhãn tiếng Việt nguyên vẹn, kèm ghi chú;
- sơ đồ hợp lệ và sơ đồ đã bọc ngoặc kép → **không** bị sửa, không ghi chú;
- nguồn vô vọng → hiện mã + lỗi gốc, không vẽ;
- lần báo chiều cao **đầu tiên** đã là số thật (696px, không còn sliver);
- theme sáng/tối cho màu node khác nhau; render lại thay chứ không xếp chồng.

8 test Flutter khoá phần định tuyến fence, gồm cả ca không nhãn và nhánh dự
phòng khi không có webview.

**Chưa có unit test cho phép sửa cú pháp.** Nó sống ở JS (desktop) và TS (web),
mà repo chưa có harness test cho cả hai. Đã kiểm bằng trình duyệt thật trên đúng
nguồn lỗi của người dùng — mạnh hơn một unit test cho việc này, nhưng vẫn là
khoảng trống nếu ai đó sửa phép sửa.

## Câu hỏi còn treo

1. Mobile (`channel_app`) chưa làm. Nó cũng có `webview_flutter`, nên cùng cách
   là khả thi — nhưng thêm 5.4 MB asset vào app điện thoại là quyết định khác
   với desktop.
2. Trần 1400px cho sơ đồ desktop là con số đặt tay. Nếu người dùng gặp flowchart
   dài hơn bị cắt, nên đổi sang cho cuộn trong khung thay vì nâng trần.
