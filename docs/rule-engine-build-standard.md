# Tiêu chuẩn build rule — Rule Engine

Tài liệu chuẩn để dựng một **rule chain** (luồng xử lý dữ liệu dạng đồ thị) trong
Space App Rule Engine (`apps/rule-engine`, port 4550, MCP server `rule-engine-mcp`).
Đây là phần thiết kế/quy ước; danh mục node chi tiết và cách gọi MCP nằm ở skill
[`apps/rule-engine/skills/rule-engine-author/SKILL.md`](../apps/rule-engine/skills/rule-engine-author/SKILL.md).

Mọi tên tool MCP dùng đầy đủ dạng `mcp__rule-engine-mcp__<tool>` (theo quy ước
đặt tên MCP của SenClaw trong `CLAUDE.md`). Không bao giờ rút gọn.

---

## 1. Nguyên tắc cốt lõi

Engine này khác engine Go gốc (`dipper-hub`) ở chỗ **định tuyến bằng cổng có
tên**, không phải bằng việc rule tự ghi id node kế tiếp.

- **Cổng có tên.** Mỗi rule khai báo cổng vào/ra trong `RuleSpec`. Rule phát ra
  *tên cổng* (`true`, `success`, `item`…); engine tra bảng `edges` để biết message
  đi tới node nào. Không nhét id node vào `config` — mọi liên kết nằm ở `edges`.
- **Mỗi cạnh một bản sao dữ liệu.** Cổng `arity: "many"` fan-out: mỗi cạnh nối ra
  nhận **một deep-clone độc lập** của message. Nhánh này sửa payload không ảnh
  hưởng nhánh kia. Cổng `arity: "one"` (nhánh quyết định như `true`/`false`) chỉ
  cho nối đúng một cạnh.
- **Cổng `error` ngầm định.** Mọi node đều có cổng ra `error` dù không khai báo.
  Message lỗi đi ra cổng này. **Không nối `error` = nhánh dừng tại đó và lỗi chỉ
  được ghi log** — không lan ngược, không làm hỏng nhánh khác.
- **Message = `{ data, meta }`.** `data` là payload phẳng; `meta` là thông tin
  kèm theo (nguồn, headers, thời điểm). Trong biểu thức, `data` nằm ở tầng trên
  cùng (dùng tên field trực tiếp), `meta` nằm dưới tên `meta_data`.
- **Run kết thúc khi hết message in-flight.** Một *Run* là một sự kiện chảy qua đồ
  thị. Nó kết thúc khi không còn message nào đang bay **và** không còn barrier nào
  đang chờ. Không có `Session` vĩnh viễn như bản Go. Trần số bước mỗi run mặc định
  10000 (`RULE_ENGINE_MAX_HOPS`) chặn vòng lặp vô tận; run quá TTL (mặc định 900s)
  bị thu hồi.

---

## 2. Quy ước đặt tên & bố cục

- **Id node**: ngắn, ổn định, chỉ chữ/số/`_`/`-` (`n1`, `check_hot`, `send_tg`).
  Id là khoá của cạnh — đổi id là gãy cạnh.
- **Tên node** (`name`): tiếng Việt ngắn gọn, mô tả việc node làm ("Nóng quá?",
  "Gửi Telegram"). Đây là nhãn hiển thị, không phải id.
- **Toạ độ.** Node rộng ~230px. Giãn `x` **320** một bước (0, 320, 640, 960…) theo
  chiều luồng chảy; giãn `y` **160** giữa các nhánh song song (0, 160, -160…). Đồ
  thị không có toạ độ hoặc giãn quá hẹp sẽ chồng lên nhau, cạnh nối ngắn tới mức
  không nhìn thấy.
- **Một node nguồn.** Mỗi chain có **đúng một** node `isSource: true` (`manual`,
  `webhook`, `schedule`, `telegram-hook`, hoặc `request` mới). Nguồn là nơi duy
  nhất khởi sinh Run.
- **Tên cổng vào của join/merge** (`config.inputs`): dùng **nguyên văn** làm khoá
  cạnh — chỉ chữ, số, `_`, `-`; không khoảng trắng, không trùng.

---

## 3. Bốn kiểu luồng dữ liệu

Rule Engine không chỉ chạy tự động theo lịch/webhook — một chain còn có thể được
app khác **gọi như một hàm**. Có bốn kiểu tương tác, mỗi kiểu gắn với một MCP tool.

> Ghi chú: `rule_trigger` là tool sẵn có để bơm một sự kiện thử vào node nguồn.
> Bốn primitive `rule_push` / `rule_call` / `rule_get` mở rộng nó cho các kiểu
> dùng dưới đây.

### 3.1 Push — bắn sự kiện, không chờ (fire-and-forget / async)

- **Khi nào**: đẩy một sự kiện vào luồng rồi trả về ngay, không cần kết quả. Đúng
  với luồng chạy nền (cảnh báo, ghi log, đồng bộ).
- **Node vào/ra**: nguồn là `manual` (hoặc bất kỳ nguồn nào). **Không cần** node
  trả kết quả.
- **MCP**: `mcp__rule-engine-mcp__rule_push {chainId, node?, port?, data, meta?}`
  — `node` mặc định là node `manual` đầu tiên; trả về ngay (kèm `runId` để soi
  trace nếu cần).

```json
{
  "nodes": [
    { "id": "n1", "rule": "manual", "name": "Nhận sự kiện", "config": {}, "x": 0, "y": 0 },
    { "id": "n2", "rule": "log",    "name": "Ghi lại",     "config": { "message": "sự kiện ${type}" }, "x": 320, "y": 0 }
  ],
  "edges": [
    { "id": "e1", "from": { "node": "n1", "port": "out" }, "to": { "node": "n2", "port": "in" } }
  ]
}
```

### 3.2 Get — đọc giá trị mới nhất (state read, không chạy)

- **Khi nào**: cần "giá trị gần nhất" mà luồng đã tính (nhiệt độ mới nhất, tồn kho
  hiện tại) mà không muốn kích hoạt một Run.
- **Node vào/ra**: đặt một node `store` ở nơi giá trị chảy qua. `store` cache
  `data` (hoặc một sub-field qua `config.key`) rồi cho đi tiếp trên `out`.
- **MCP**: `mcp__rule-engine-mcp__rule_get {chainId, node}` — đọc thẳng giá trị
  `store` đang giữ, **không tạo Run**.

```json
{
  "nodes": [
    { "id": "n1", "rule": "schedule", "name": "Mỗi phút",  "config": { "cron": "0 * * * * *" }, "x": 0,   "y": 0 },
    { "id": "n2", "rule": "http-request", "name": "Đo",     "config": { "method": "GET", "url": "https://..." }, "x": 320, "y": 0 },
    { "id": "n3", "rule": "store", "name": "Giữ mới nhất",  "config": { "key": "body.temp" }, "x": 640, "y": 0 }
  ],
  "edges": [
    { "id": "e1", "from": { "node": "n1", "port": "out" },     "to": { "node": "n2", "port": "in" } },
    { "id": "e2", "from": { "node": "n2", "port": "success" }, "to": { "node": "n3", "port": "in" } }
  ]
}
```

Người gọi sau đó: `rule_get {chainId, node: "n3"}`.

### 3.3 Pull — gọi và chờ kết quả (request / response, đồng bộ)

- **Khi nào**: app khác muốn gọi chain như một API và **chờ kết quả**.
- **Node vào/ra**: nguồn là `request` (điểm vào có tên). Luồng phải kết thúc ở
  **đúng một** node `respond`; bất cứ gì tới `respond` trở thành kết quả trả về.
- **MCP**: `mcp__rule-engine-mcp__rule_call {chainId, node?, data, timeoutMs?}`
  — bơm dữ liệu vào `request`/`manual`, **chờ** Run chạm `respond`, trả
  `{status, result, error}`. `node` bỏ trống = node `request` đầu tiên.

```json
{
  "nodes": [
    { "id": "n1", "rule": "request",   "name": "Vào",       "config": {}, "x": 0,   "y": 0 },
    { "id": "n2", "rule": "arithmetic","name": "Tính",      "config": { "operators": [ { "target": "vat", "expr": "gia * 0.1" } ] }, "x": 320, "y": 0 },
    { "id": "n3", "rule": "respond",   "name": "Trả kết quả","config": {}, "x": 640, "y": 0 }
  ],
  "edges": [
    { "id": "e1", "from": { "node": "n1", "port": "out" }, "to": { "node": "n2", "port": "in" } },
    { "id": "e2", "from": { "node": "n2", "port": "out" }, "to": { "node": "n3", "port": "in" } }
  ]
}
```

Người gọi: `rule_call {chainId, data: {"gia": 100}}` → nhận `{status, result: {gia:100, vat:10}}`.

### 3.4 Callback — luồng gọi ra ngoài (round-trip trong một Run)

- **Khi nào**: trong lúc chạy, luồng cần gọi ra một dịch vụ ngoài rồi dùng kết quả
  đi tiếp; hoặc chuỗi này gọi sang một chain khác.
- **Node**: `http-request` (gọi API ngoài, `{status, body, headers}` ra cổng
  `success`/`failed`) hoặc `mcp-call` (gọi tool MCP của Space App khác, gắn kết quả
  vào `outputField`).
- **Đối xứng**: bất kỳ chain nào có `request` + `respond` đều có thể được một app
  khác (hoặc một node `mcp-call`) gọi lại qua `rule_call` — nên callback và pull
  là hai đầu của cùng một cơ chế.

---

## 4. Chuẩn xử lý lỗi

- **Dùng cổng `error`.** Muốn bắt lỗi thì nối cổng `error` của node sang nhánh xử
  lý (thông báo, retry, ghi tri thức). Không nối = nhánh dừng, lỗi vào log.
- **Phân biệt "lỗi dữ liệu" với "một nhánh hợp lệ".** Ví dụ `switch`: thiếu field
  `key` → cổng `error` (lỗi), không có case khớp → cổng `default` (nhánh hợp lệ).
- **`finish == "length"` của AI là LỖI.** Node `ai-agent` coi câu trả lời bị cắt
  vì chạm trần token là lỗi và đẩy ra cổng `error`, không đi tiếp với JSON gãy.
  Đặt `maxTokens` đủ lớn.
- **Biểu thức lỗi = cổng `error`.** `conditional`/`arithmetic`/`project` gặp lỗi cú
  pháp, chia 0, hay không trả boolean/số đều đẩy cả message ra `error` kèm tên
  field — không âm thầm rơi vào `false` hay ghi kết quả nửa vời.
- **Guardrail của luồng đồng bộ.** `rule_call` trả `{status, error}`; luồng pull
  nên có một nhánh từ `error` tới một `respond` (hoặc để `respond` nhận cả nhánh
  lỗi) để người gọi luôn nhận được câu trả lời thay vì timeout.

---

## 5. Gộp nhiều nhánh (nhiều cổng vào)

Mặc định **mỗi message vào một node là một lần chạy riêng**. Hai cạnh trỏ vào cùng
một node = node đó chạy **hai lần**, không phải gộp.

Muốn chờ đủ rồi mới chạy một lần, dùng `join` hoặc `merge` và **bắt buộc**:

1. Khai `config.inputs` = danh sách tên cổng vào (mỗi tên tạo một cổng, dùng
   nguyên văn làm khoá cạnh).
2. Đặt `opts.join`:
   - `"all"` (cho `join`) — chờ đủ mỗi cổng, phát `{ "<tên cổng>": <data>, ... }`.
   - `"merge"` (cho `merge`) — như trên nhưng deep-merge phẳng thành một object.

> ⚠️ **Mặc định `opts.join = "any"` KHÔNG bật rào chắn.** Giao diện tự đặt
> `opts.join` khi kéo node `join`/`merge` ra canvas, nhưng khi dựng qua MCP
> (`rule_update_graph`) **bạn phải tự đặt** — để `"any"` thì node chạy một lần cho
> mỗi cạnh vào (hai nhánh → node sau chạy hai lần với dữ liệu chưa gộp), **không có
> cảnh báo nào**.

```json
{ "id": "j1", "rule": "join", "name": "Chờ đủ",
  "config": { "inputs": ["thoi_tiet", "ton_kho"] },
  "opts": { "join": "all", "joinTimeoutMs": 30000 }, "x": 640, "y": 0 }
```

- **`opts.joinTimeoutMs`** — quá hạn mà chưa đủ nhánh thì phần đã nhận bị huỷ và
  ghi log; run không treo mãi (mặc định `RULE_ENGINE_JOIN_TIMEOUT_MS` = 60000).
  Đặt nó khi một nhánh có thể không bao giờ tới.
- **`opts.corrKey`** — gộp theo một giá trị nghiệp vụ trong dữ liệu (vd `order_id`)
  thay vì theo lượt chạy; cần khi nhiều item chạy song song qua cùng barrier.
- Một nhánh lỗi làm cả message gộp thành lỗi, đi ra cổng `error`.

---

## 6. Luồng logic đặc biệt

### dedup — chặn trùng lặp

`config.key` (đường dẫn field; bỏ trống = so toàn bộ `data`), `windowMs`
(mặc định 60000). Lần đầu → cổng `out`; lần lặp trong cửa sổ → cổng `dropped`.
Dùng để chống double-fire (webhook gửi lại, sự kiện dội).

### rate-limit — giới hạn tần suất

Token bucket. `config.rate` (mặc định 5), `perMs` (mặc định 1000). Trong hạn →
cổng `out`; vượt hạn → cổng `dropped`. Dùng để bảo vệ API downstream.

### split + aggregate — tách rồi gom (cặp đôi)

- `split` biến một mảng thành N message trên cổng `item`, cộng một message
  `{ count: N }` trên cổng `done`.
- `aggregate` là chiều ngược lại: cổng vào `in` tích luỹ, cổng vào `flush` ép phát.
  `config.count` (mặc định 10; `0` = chỉ flush thủ công). Phát `{ items: [...],
  count: N }` trên `out`.

```
mảng ──▶ split ──item──▶ (xử lý từng phần tử) ──▶ aggregate ──out──▶ gom lại
                └─done──────────────────────────▶ aggregate(flush)
```

Nối cổng `done` của `split` vào cổng `flush` của `aggregate` để gom đúng một lô
theo từng mảng thay vì đoán số lượng.

### delay — chèn khoảng nghỉ

`config.ms` (mặc định 1000, tự cắt 0–300000). Dữ liệu ra y hệt vào. ⚠️ Node chiếm
một worker suốt thời gian chờ; với `opts.concurrency = 1` (mặc định) message xếp
hàng. Cần chờ hàng phút/giờ hãy dùng nguồn `schedule`, đừng dùng `delay`.

### trigger-time — so thời gian, rẽ nhánh

So một thành phần thời gian (`minute`/`hour`/`day`/`weekday`/`month`/`year`) của
hai mốc (`left`, `right`; nhận `now()`, unix giây, hoặc chuỗi RFC3339) theo
`timezone`, ra cổng `true`/`false`. Hỏi "đã tới giờ chưa?" mà không cần viết biểu
thức thời gian. Múi giờ quyết định thật sự kết quả với `day`/`weekday`.

### store — giữ giá trị mới nhất

`config.key` (sub-field muốn cache; bỏ trống = cả `data`). Passthrough trên `out`,
đọc lại bằng `rule_get`. Xem §3.2.

---

## 7. Checklist trước khi kích hoạt

Gọi `rule_validate` và sửa hết lỗi (`level: "error"`) trước khi `rule_activate`.
Kiểm bằng mắt các điểm sau:

- [ ] **Đồ thị validate sạch** — không còn `level: "error"`. Cảnh báo (`warning`)
      đọc rồi tự quyết.
- [ ] **Có đúng một node nguồn** (`isSource: true`) và nó đã nối đi đâu đó (nguồn
      không nối = sự kiện rơi vào hư vô).
- [ ] **Nếu luồng sẽ được gọi đồng bộ** (`rule_call`) → có nguồn `request` và kết
      thúc ở **đúng một** node `respond`.
- [ ] **Mọi `join`/`merge` đã đặt `opts.join`** = `"all"`/`"merge"` (không để mặc
      định `"any"`), `config.inputs` khớp đúng các cạnh thực nối vào, và cân nhắc
      `opts.joinTimeoutMs` nếu một nhánh có thể không tới.
- [ ] **Tên cổng đúng** — mỗi `port` trong `edges` nằm trong danh sách in/out của
      node (lấy từ `rule_registry`). Cổng `error` luôn dùng được.
- [ ] **Cổng `arity: "one"`** (`true`/`false`, `success`/`failed`, `pass`/`noise`)
      không bị nối 2 cạnh.
- [ ] **Không vòng lặp thiếu điều kiện thoát** — vòng lặp chỉ là cảnh báo, nhưng
      phải có nhánh dừng, nếu không sẽ chạm trần số bước (`RULE_ENGINE_MAX_HOPS`).
- [ ] **Bí mật do người dùng cung cấp** (bot token, API key) — đừng bịa.

Deep-dive từng node: [`SKILL.md`](../apps/rule-engine/skills/rule-engine-author/SKILL.md).
Chẩn đoán khi luồng không chạy: skill `rule-engine-debug`.
