# Ngôn ngữ giao diện web

Web UI trước đây chỉ có tiếng Anh, trong khi desktop đã có bản dịch đầy đủ —
và người dùng chính nói tiếng Việt.

## Một từ điển, hai client

**Chuỗi tiếng Anh chính là khoá.** `t('Settings')` trả bản dịch khi ngôn ngữ là
`vi`, còn lại trả chính nó. Thiếu bản dịch thì hiện tiếng Anh đọc được, không
phải khoá thô — và thêm câu mới không cần bịa ra một khoá cho nó.

Đây đúng là hợp đồng của desktop (`context.tr(...)` trong
`desktop_app/lib/core/i18n/l10n.dart`), và **hai bên đọc chung một từ điển**:

```
desktop_app/lib/core/i18n/vi/*.dart   ← nguồn (2 091 chuỗi)
        │  dart run tool/i18n_export.dart
        ▼
web/src/i18n/vi.json                  ← sinh ra, ĐỪNG sửa tay
web/src/i18n/vi.web.json              ← câu chỉ web nói, sửa tay ở đây
```

Script import chính các map Dart rồi `jsonEncode`, **không parse văn bản** —
nên thứ web nhận đúng bằng thứ desktop hiển thị, kể cả chỗ Dart nối chuỗi liền
nhau mà một parser văn bản sẽ đọc sai.

`vi.web.json` thắng khi trùng khoá, để một cách diễn đạt riêng của web không bị
bản desktop lặng lẽ thay mất.

## Vì sao không dùng thư viện i18n

Kế hoạch ban đầu ghi `react-i18next`. Khi khoá đã là câu tiếng Anh, chỉ có một
ngôn ngữ thứ hai, và tiếng Việt không biến đổi theo số nhiều, thì toàn bộ yêu
cầu là một provider và một phép tra bảng — khoảng 70 dòng ở
[`web/src/i18n/index.tsx`](../web/src/i18n/index.tsx). Thêm dependency ở đây
làm nặng bundle và đem vào bộ quy ước thứ hai mà **không thay thế được gì**.
Ghi lại ở đây vì đây là chỗ lệch so với kế hoạch, không phải việc bỏ sót.

## Dùng

```tsx
import { useLang } from '../i18n';

const { t, lang, setLang } = useLang();
<Text>{t('Settings')}</Text>
```

`useLang()` gọi được cả ngoài provider — nó lùi về tiếng Anh thay vì ném lỗi,
nên một component render biệt lập (test, portal) vẫn có chữ.

Lựa chọn ngôn ngữ lưu ở `localStorage` (`senclaw.lang`), mặc định theo
`navigator.language`. Đổi ở **Settings → Permissions → Language**. Mọi lần đọc
/ ghi `localStorage` đều bọc `try` — chế độ ẩn danh và trình duyệt chặn site
data đều ném lỗi ở đó.

## Kiểm

```bash
scripts/i18n-check.sh          # báo cáo
scripts/i18n-check.sh --strict # đỏ khi còn chuỗi chưa dịch
```

Hai loại phát hiện, và chỉ một loại là lỗi:

- **Chưa có bản dịch** — *không* phải lỗi. Câu đó hiện tiếng Anh, đúng thiết kế.
- **Khoá trong `vi.web.json` mà không code nào dùng** — **là lỗi**: một câu đã
  bị sửa lời và bỏ lại bản dịch cũ, nên lần sau ai đó sẽ dịch lại từ đầu.

## Đã dịch tới đâu

Hạ tầng xong; phần chữ mới chuyển một phần. Hiện tại: **Settings shell**
(tiêu đề, menu) và **General settings**. Các trang còn lại vẫn tiếng Anh — và
vì tiếng Anh là khoá, chúng hiển thị đúng chứ không vỡ.

Làm tiếp theo lô, mỗi lô một trang, chạy `scripts/i18n-check.sh` sau mỗi lô.
Thứ tự đề xuất theo lượng chữ: Chat → Plugins → Space → Cognitive.

## Khi sửa một câu tiếng Anh

Sửa chuỗi trong code **và** khoá tương ứng. Nếu câu đó thuộc từ điển chung, sửa
trong `desktop_app/lib/core/i18n/vi/*.dart` rồi chạy lại export:

```bash
cd desktop_app && dart run tool/i18n_export.dart
```

`scripts/i18n-check.sh` sẽ bắt được nếu quên — khoá cũ thành mồ côi.
