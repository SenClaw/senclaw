# Rule Engine Space App — thiết kế migration

> Trạng thái: **nghiên cứu / đề xuất thiết kế**, chưa viết code.
> Ngày: 2026-07-20
> Nguồn: `light-cart/dipper-hub/services/engine-runner` (Go, 7.3k LOC) + thư viện lõi
> `github.com/dipper-iot/dipper-engine@v0.0.9` (~4.9k LOC) + `dipper-hub/frontend/src/modules/rule_chan`
> (~8.2k LOC TS).

---

## 1. Mục tiêu & phạm vi

Tạo `apps/rule-engine` — một SenClaw Space App độc lập, port toàn bộ rule engine của
Dipper sang Rust, kèm UI canvas lấy từ `frontend/src/modules/rule_chan`.

Yêu cầu bổ sung của người dùng: **"đảm bảo migration toàn bộ sang hỗ trợ input–output
nhiều phần"** — tức mô hình nhiều cổng vào / nhiều cổng ra phải là *first-class*, không
phải hack theo từng rule như bản gốc.

### 1.1 Vì sao không dùng `src/workflow` của core

`src/workflow` là DAG **batch, chạy một lần**: step kiểu agent/script, phụ thuộc bằng
`depends_on`, kết quả là `String`, có `WorkflowRun` với trạng thái pending/running/done.
Rule engine của Dipper là **streaming, event-driven**: chain thường trú, mỗi sự kiện bên
ngoài bơm một message chảy qua đồ thị, node chọn nhánh theo điều kiện, có vòng lặp, có
node giữ state (kalman, moving average). Hai mô hình không giao nhau — tách app riêng.

### 1.2 Vì sao không dùng `apps/hub`

`apps/hub` là HMI client mỏng gọi GraphQL của Dipper hub từ xa (6 MCP tool). Nó là *nguồn
dữ liệu*, không phải engine. Rule engine sẽ gọi sang nó qua app→app MCP (§7.4).

---

## 2. Bản chất engine gốc — những gì phải biết trước khi port

Đây là phần quan trọng nhất: **bản gốc không có khái niệm "port"**.

### 2.1 Routing hiện tại

`OutputEngine.Next []string` là tất cả. Rule tự đọc option của mình để biết node kế tiếp
rồi ghi thẳng node-id vào `Next`. Engine chỉ làm: lọc chuỗi rỗng → nếu rỗng thì kết thúc
nhánh, ngược lại tra `MapNode[nextId]` và đẩy message vào queue của `rule_id` đó.

Nghĩa là "cổng ra" chỉ là **quy ước đặt tên field trong `option` JSON**, mỗi rule một kiểu:

| Rule | Field trong `option` | Kiểu |
|---|---|---|
| arithmetic, fork, format, http-request, notification, output-* | `next_success`, `next_error` | array hoặc string hoặc JSON-string |
| conditional | `next_true`, `next_false`, `next_error` | array |
| project, trigger_time_set | `next_true`, `next_false`, `next_error` | **string** (1 node) |
| switch | `map_switch` | `map[case]nodeId` |
| moving_average, kalman | `next_success`, `next_failed`, `next_error` | array |

Và các field đó **không nằm trong DB** — chúng được `usecase/rule_chan/maping.go:99-131`
*inject vào lúc chạy* từ `connect_to`, với logic hard-code chỉ hiểu đúng 3 trường hợp:

```go
case "arithmetic":  mapData["next_success"] = &lt;mọi target&gt;
case "conditional": yes → next_true, no → next_false
case "switch":      mapSwitch[sourceHandle] = target
default:            mapData["next_success"] = &lt;mọi target&gt;
```

### 2.2 Hệ quả: các bug port-model đang tồn tại

1. **`success`/`failed` của `http-request`, `kalman_filter`, `moving_average_filter` bị gộp
   làm một.** UI vẽ 2 handle, backend rơi vào `default:` → cả hai đều vào `next_success`.
2. **`project`, `trigger_time_set` không bao giờ được nối tự động** — chúng cần
   `next_true`/`next_false` dạng *string*, `maping.go` không sinh ra.
3. **`switch` hoàn toàn không chạy**: `ToOption()` parse `map_switch` từ nhầm field
   (`ToStringNextSuccess`), kiểu khai `string` trong khi DB lưu object → `MapToStruct` fail
   → luôn rơi nhánh error, mà `NextError` cũng không được gán → `Next=[""]` → chain chết.
   Thêm nữa vòng lặp so khớp đảo key/value và `numberData` không bao giờ được gán.
4. **`log`, `telegram-send`, `ai-agent`, `output-model`, `output-action` không publish
   output** → chain dừng hẳn sau các node đó.
5. **Lỗi parse option ⇒ `Next` không được set ⇒ nhánh chết im lặng**, không đi qua
   `next_error`. Có ở mọi rule.
6. **Không có multi-input.** Mỗi message tới node là một lần chạy độc lập. `fork` → 2 nhánh
   → gộp về 1 node = node đó chạy **2 lần**, không merge.
7. Edge được lưu **lặp 2 lần** (ở cả node nguồn lẫn node đích) dưới dạng mảng chuỗi
   JSON trong `rule_nodes.connect_to`.

### 2.3 Các bug runtime khác (không thuộc port model)

| Vấn đề | Vị trí |
|---|---|
| `timeout_session: 30` bị ép sang `time.Duration` = **30 nanosecond**; và không code nào đọc field đó → **không có timeout session** | `ENGINE/core/session.go:91` |
| `EndCount` đếm tĩnh số node `end:true` nhưng bị trừ động mỗi message terminal → fork hội tụ làm nó âm; node `end:false` cũng trừ | `ENGINE/store/default.go:36-68` |
| `Clone()` là shallow — fan-out N nhánh dùng chung một map → data race in-process | `ENGINE/data/ouput.go:30-44` |
| `Rule.Stop()` **không bao giờ được engine gọi** | `ENGINE/core/dipper.go:77-80` |
| Default queue: `Publish` = `go func(){ ch <- x }()` → không backpressure, không thứ tự, leak goroutine | `ENGINE/queue/default.go` |
| Redis queue dùng `RPop` không sleep, không `BRPop` → busy-spin | `ENGINE/redis/queue.go:53-100` |
| Hub Redis là **Pub/Sub** → `worker > 1` sẽ xử lý trùng message | `pkg/hub/redis.go:38-93` |
| `StartChanId` early-return nếu chain đã chạy → **reload không cập nhật chain đang chạy** | `usecase/rule_chan/rule_start.go:73-75` |
| `StopSession(chanId)` gọi `ControlSession(ruleName)` sai tham số → stop không hoạt động | `usecase/runner/runner.go:83-93` |
| telegram-hook / webhook tạo `OutputEngine` mới với `SessionId=0` → router drop toàn bộ | `usecase/{telegram,webhook}_source/service.go` |
| `schedule` không truyền `SessionId` → cron của chain thứ 2 đè chain thứ 1 | `pkg/inputs/schedule/schedule_rule.go:52-59` |
| `base.ToDebug()` hard-code `false` | `pkg/rules/base/option.go:134-142` |
| `util.GetTime` chỉ nhận `int64`, sau JSON là `f64` → luôn trả `now()` | `pkg/util/rule.go:80-92` |
| `usecase/action.Action()` là **stub trả nil** → `output-action` là no-op | `usecase/action/service.go:18-27` |
| `worker_infinity: true` trong `engine.json` là **no-op** (field không tồn tại trong v0.0.9) | `engine.json:27` |
| `get-last-model` khai `Infinity()=true` → mọi chain chứa nó không bao giờ `Done` | `pkg/rules/get_last_model/get_last_model.go:19` |
| UI dùng `http_request`, backend dùng `http-request`; UI có node `delay` không có backend; UI `format` lưu sai option (`model_id`) | — |
| `ENGINE/rules/output_redis_queue` **không compile** (thiếu import redis) → code chết | `ENGINE/rules/output_redis_queue/input_redis_queue.go:14` |

> Kết luận: đây **không phải** một port 1:1. Phần lớn "tính năng" multi-port là bug. Bản Rust
> phải *thiết kế lại* lớp routing, giữ nguyên ngữ nghĩa của từng rule.

---

## 3. Kiến trúc đích

```
apps/rule-engine/                        port 4550
├── src/
│   ├── main.rs            bootstrap axum, static dir, health ngay lập tức
│   ├── config.rs          mọi env::var()
│   ├── state.rs           Core { db, engine, registry, bus }
│   ├── db.rs              rusqlite, SCHEMA + MIGRATIONS
│   ├── api.rs             REST + SSE
│   ├── mcp.rs             JSON-RPC MCP over HTTP/SSE
│   ├── engine/
│   │   ├── types.rs       Message, PortRef, Edge, Graph, RunCtx
│   │   ├── graph.rs       load/validate/compile đồ thị
│   │   ├── scheduler.rs   per-node mailbox, worker, backpressure
│   │   ├── router.rs      (node, out_port) → edges → (node, in_port)
│   │   ├── join.rs        join buffer + policy + TTL
│   │   ├── run.rs         vòng đời run/trace, reaper
│   │   └── registry.rs    RuleSpec registry (cho UI + validate)
│   ├── expr/              biểu thức (govaluate-compat) + DAQ path
│   ├── rules/             1 file / rule
│   ├── inputs/            source node
│   └── state_store.rs     state per-node (thay Redis)
├── web/                   React 19 + AntD 6 + @xyflow/react 12
├── skills/  personas/  scripts/pack.sh  senclaw-manifest.json
```

**Ràng buộc từ nền tảng Space App** (đã xác minh trong core):

- Daemon health-gate **30 giây** (`120 × 250ms`) → phải bind port và trả 200 ở
  `/api/status` **trước khi** nạp/khởi động chain. Engine boot bất đồng bộ sau listener.
- DB phải nằm **ngoài** thư mục cài: `~/.senclaw/space-app-data/rule-engine/app.sqlite`
  (cài lại zip sẽ xoá sạch `app_dir`).
- `vite base: './'` bắt buộc — UI serve ở 2 base (`:4550/` và `/api/space/apps/rule-engine/proxy/`).
- MCP tự viết JSON-RPC over HTTP/SSE bằng axum (không dùng `rmcp`); tool prefix `rule_*`,
  server `rule-engine-mcp`, path `/api/mcp/sse`.
- Bridge `llm.request` **không có `temperature`** (hard-code 0.2) và `finish == "length"`
  phải coi là **lỗi**. `mcp.call` vẫn là stub → app→app phải POST thẳng `/api/mcp/message`.
- Port 4550 (4540 đã bị `apps/json` chiếm).

---

## 4. Mô hình port tổng quát — thiết kế mới

Đây là trọng tâm yêu cầu. Ba thay đổi nền tảng:

### 4.1 Edge là first-class, config không còn chứa node-id

Bản gốc: rule tự biết node kế tiếp (node-id nằm trong `option`). Bản mới: **rule không biết
gì về topology**, nó chỉ phát ra một *tên cổng*; engine tra bảng edge.

```rust
pub struct PortRef { pub node: NodeId, pub port: PortId }

pub struct Edge {
    pub id: EdgeId,
    pub from: PortRef,   // (node, out_port)
    pub to:   PortRef,   // (node, in_port)
}
```

Lưu ở **bảng `edges` riêng**, không nhúng vào node, không lặp 2 lần. `node.config` trở
thành cấu hình thuần tuý (không còn `next_success`/`next_true`/`map_switch`). Toàn bộ
`maping.go` biến mất.

### 4.2 Rule khai báo port tĩnh + động

```rust
pub enum PortArity { One, Many }        // Many = cho phép nhiều edge cùng cổng (fan-out)
pub enum JoinPolicy { Any, All, Merge } // xem §4.4

pub struct PortSpec {
    pub id: PortId,            // "in", "out", "yes", "no", "success", "failed", "error"
    pub label: String,         // hiển thị cạnh handle trên canvas
    pub color: Option<String>,
    pub arity: PortArity,
}

pub struct RuleSpec {
    pub id: &'static str,                  // "conditional"
    pub category: Category,                // Source | Transform | Logic | Sink | Ai
    pub inputs:  Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
    pub config_schema: serde_json::Value,  // JSON Schema → UI tự sinh form
}

pub trait Rule: Send + Sync {
    fn spec(&self) -> &RuleSpec;
    /// Cổng phụ thuộc config (switch: mỗi case một cổng).
    fn dynamic_outputs(&self, _cfg: &Value) -> Vec<PortSpec> { vec![] }
    async fn init(&self, _cfg: &Value) -> Result<()> { Ok(()) }
    async fn handle(&self, ctx: &RunCtx, msg: Message) -> Outcome;
    async fn stop(&self) -> Result<()> { Ok(()) }   // engine PHẢI gọi (gốc không gọi)
}
```

`RuleSpec` được expose qua `GET /api/registry` → UI **tự sinh** handle và form config từ
đó. Không còn 24 file TS hard-code handle như bản gốc.

### 4.3 Nhiều output: `Outcome::Emit(Vec<Emission>)`

```rust
pub struct Emission { pub port: PortId, pub data: Value, pub meta: Option<Value> }

pub enum Outcome {
    Emit(Vec<Emission>),   // 0..N message, có thể nhiều message trên CÙNG một cổng
    Terminal,              // kết thúc nhánh có chủ đích
    Fail(EngineError),     // → cổng "error" ngầm định
}
```

Ba khả năng mới so với bản gốc:

- **Multi-port emit**: một lần chạy phát ra nhiều cổng khác nhau (ví dụ `tee`, hoặc
  `http-request` phát `success` + đồng thời `raw` để log).
- **Multi-message trên một cổng**: node `split` nhận mảng, phát N message trên `item` →
  vòng lặp thật sự, thứ mà bản gốc không có.
- **Cổng `error` ngầm định**: mọi node đều có. Nếu không nối → nhánh kết thúc **và ghi
  log lỗi** (sửa bug "chết im lặng"). Nếu nối → message mang `kind: Error` chảy tiếp.

### 4.4 Nhiều input: join có chính sách

Mỗi input port có `JoinPolicy` (cấu hình ở node, không phải rule):

| Policy | Ngữ nghĩa |
|---|---|
| `Any` (mặc định) | Mỗi message tới là một lần chạy độc lập — **tương thích ngược 100%** với bản gốc |
| `All` | Chờ đủ một message trên **mỗi input port đang được nối**, rồi chạy 1 lần với `data = { "&lt;port_id&gt;": &lt;data&gt;, ... }` |
| `Merge` | Như `All` nhưng deep-merge các `data` thành một object phẳng |

Buffer join keyed theo `(run_id, node_id, epoch)`:

- `run_id` — mỗi **sự kiện bên ngoài** tạo một run mới (§4.5). Không dùng `session_id`
  vĩnh viễn như bản gốc.
- `epoch` — bộ đếm tăng khi node đã đủ input và fire; cho phép vòng lặp và luồng lặp lại.
- Tuỳ chọn `corr_key: "&lt;json path&gt;"` để gom theo khoá nghiệp vụ (ví dụ `device_id`) thay
  vì theo run.
- **TTL bắt buộc** (mặc định 60s, cấu hình được): join dở dang quá hạn bị thu hồi và phát
  ra cổng `error` với `join_timeout`. Bản gốc không có gì tương đương.

Kèm 3 node mới built-in: `join` (barrier thuần), `merge` (gộp object), `split` (mảng → N message).

### 4.5 Run thay cho Session

Bản gốc: một chain = một `Session` vĩnh viễn, `EndCount` đếm sai, `Infinite=true` khiến
session không bao giờ kết thúc.

Bản mới tách đôi:

- **Deployment** — chain ở trạng thái `ACTIVE` được nạp một lần, source node đăng ký
  lắng nghe. Tồn tại đến khi chain bị stop/sửa.
- **Run** — mỗi sự kiện từ source tạo một `run_id` (snowflake). Run kết thúc khi **không
  còn message in-flight và không còn join đang chờ**. Đây là điều kiện đúng, thay cho
  `EndCount`. Run có TTL + reaper; run quá hạn bị đánh dấu `timeout`.
- **Trace** — mỗi hop (`from_port → to_port`, data trước/sau, thời gian, lỗi) ghi vào
  `run_hops` khi node bật `debug` hoặc chain bật debug. Đây là thứ nuôi UI debug console
  realtime (§8.4).

### 4.6 Message

```rust
pub struct Message {
    pub run_id: u64,
    pub chain_id: i64,
    pub seq: u64,                    // tăng đơn điệu trong run
    pub target: PortRef,             // (node, in_port)
    pub from: Option<PortRef>,
    pub epoch: u32,
    pub branch: String,              // = BranchMain cũ, mặc định "default"
    pub data: Value,
    pub meta: Value,
    pub kind: MsgKind,               // Data | Error
    pub error: Option<EngineError>,
    pub ts: i64,
}
```

**Data ownership**: bản gốc share `map` giữa N nhánh fan-out → data race. Bản Rust dùng
`Arc<Value>` cho nhánh đơn và **deep clone tại điểm fan-out** (khi một cổng có >1 edge
hoặc emit nhiều cổng). Đây là sửa bug, không phải thay đổi ngữ nghĩa mong muốn.

### 4.7 Scheduler

Bản gốc: 1 queue cho mỗi **rule type**, 2 node cùng `rule_id` chia chung queue + pool.

Bản mới: **1 mailbox (`tokio::mpsc` bounded) cho mỗi node**, `concurrency` cấu hình ở node
(mặc định 1 → giữ thứ tự trong node). Một `Semaphore` toàn cục chặn bùng nổ. `send().await`
cho backpressure thật. Thay `Ack/Reject` bằng `Result` trả về từ handler + retry policy
theo node (`retries`, `backoff`).

---

## 5. Bảng ánh xạ cổng — 24 rule sau khi thiết kế lại

Cột "Gốc" = hành vi hiện tại; cột "Mới" = cổng khai báo trong `RuleSpec`.

### 5.1 Logic / Transform

| Rule | In | Out (mới) | Gốc | Ghi chú port |
|---|---|---|---|---|
| `arithmetic` | `in` | `out`, `error` | `next_success`/`next_error` | giữ nguyên |
| `conditional` | `in` | `true`, `false`, `error` | `next_true`/`next_false`/`next_error` (handle UI `yes`/`no`) | đổi tên handle `yes/no` → `true/false`, importer map lại |
| `switch` | `in` | **N cổng động** = mỗi case + `default` + `error` | `map_switch` (hỏng hoàn toàn) | `dynamic_outputs()` sinh từ `cases: [{value, port_label}]` |
| `fork` | `in` | `out` (arity **Many**) | `next_success []` | fan-out bằng nhiều edge trên cùng cổng |
| **`split`** *(mới)* | `in` | `item`, `done`, `error` | — | mảng → N message trên `item` |
| **`join`** *(mới)* | `a`, `b`, … (N, cấu hình) | `out`, `error` | — | policy `All`, có TTL |
| **`merge`** *(mới)* | `a`, `b`, … | `out`, `error` | — | deep-merge |
| `format` | `in` | `out`, `error` | `next_success`, **không dùng** `next_error` | thêm nhánh error thật |
| `project` | `in` | `out`, `error` | `next_true`/`next_error` (string, không tự nối được) | đổi `next_true` → `out`; thêm `set_string` bị thiếu trong switch gốc |
| `trigger_time_set` | `in` | `true`, `false`, `error` | string, không tự nối được | thêm `timezone` (gốc nhận nhưng không dùng) |
| `delay` *(UI có, backend không)* | `in` | `out` | — | hiện thực thật |
| `log` | `in` | `out`, `error` | **không publish** → chain dừng | thêm `out` pass-through |

### 5.2 Filter có state

| Rule | In | Out | Gốc | State |
|---|---|---|---|---|
| `moving_average_filter` | `in` | `pass`, `noise`, `error` | `next_success`/`next_failed`/`next_error` — **bị gộp bởi `maping.go`** | Redis list → bảng `node_state` (SQLite) + cache in-memory |
| `kalman_filter` | `in` | `out`, `error` | `next_failed` khai nhưng không dùng | Redis JSON `{x,p}` → `node_state` |

### 5.3 I/O

| Rule | In | Out | Gốc |
|---|---|---|---|
| `http-request` | `in` | `success`, `failed`, `error` | 2 cổng UI bị gộp; non-2xx đi `next_error` nhưng `Type=success` |
| `telegram-send` | `in` | `out`, `error` | không publish → chain dừng |
| `create_notification` | `in` | `out`, `error` | `next_success` |
| `output-action` | `in` | `out`, `error` | **stub no-op**, không publish |
| `output-model` | `in` | `out`, `error` | không publish |
| **`mcp-call`** *(mới)* | `in` | `out`, `error` | — | gọi MCP tool của app khác |
| **`senclaw-send`** *(mới)* | `in` | `out`, `error` | — | gửi qua channel của core |

### 5.4 AI

| Rule | In | Out | Gốc |
|---|---|---|---|
| `ai-agent` | `in` | `out`, `error` | **không set `next` bao giờ** → luôn kết thúc nhánh |
| **`ai-persona`** *(mới)* | `in` | `out`, `error` | — | `agent.run` với persona + tool allowlist |
| **`knowledge`** *(mới)* | `in` | `out`, `error` | — | `knowledge.save` / `search` / `recall` |

### 5.5 Source (input rule)

Source không có input port; mỗi lần phát tạo một `run_id` mới.

| Rule | Out | Gốc |
|---|---|---|
| `webhook` | `out`, `error` | `next_success`; **`SessionId=0` → message bị drop** |
| `schedule` | `out` | id thật là `input-schedule` (engine.json ghi `schedule`); cron đè nhau giữa các chain |
| `telegram-hook` | `out` | cùng bug `SessionId=0` |
| `input-telemetry` | `out` | hub topic `device.log.parsed` — **cần quyết định §7.3** |
| `input-model` | `out` | hub topic `model.log.updated` — nt |
| `input-stream` | `out` | hub topic `device.stream.raw` — nt |
| `get-last-model` | `out`, `error` | `Infinity()=true` làm hỏng vòng đời session — nt |
| **`manual`** *(mới)* | `out` | — | kích hoạt tay từ UI/MCP để test chain |
| **`mqtt`** *(đề xuất)* | `out`, `error` | — | thay hub Kafka/Redis cho telemetry push |

---

## 6. Lược đồ dữ liệu

```sql
CREATE TABLE chains (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  description TEXT,
  status TEXT NOT NULL DEFAULT 'INACTIVE',   -- ACTIVE | INACTIVE | ERROR
  debug INTEGER NOT NULL DEFAULT 0,
  version INTEGER NOT NULL DEFAULT 1,
  created_at TEXT, updated_at TEXT
);

CREATE TABLE nodes (
  id TEXT NOT NULL,                 -- node id trong đồ thị
  chain_id INTEGER NOT NULL,
  rule TEXT NOT NULL,               -- RuleSpec.id
  name TEXT NOT NULL,
  config TEXT NOT NULL DEFAULT '{}',-- JSON thuần config, KHÔNG chứa node-id
  ports TEXT NOT NULL DEFAULT '{}', -- override join policy / corr_key / concurrency
  x REAL NOT NULL DEFAULT 0,        -- toạ độ là CỘT, không nhét vào config
  y REAL NOT NULL DEFAULT 0,
  debug INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chain_id, id)
);

CREATE TABLE edges (
  id TEXT NOT NULL,
  chain_id INTEGER NOT NULL,
  from_node TEXT NOT NULL, from_port TEXT NOT NULL,
  to_node   TEXT NOT NULL, to_port   TEXT NOT NULL,
  PRIMARY KEY (chain_id, id)
);
CREATE INDEX idx_edges_from ON edges(chain_id, from_node, from_port);

CREATE TABLE runs (
  id INTEGER PRIMARY KEY, chain_id INTEGER NOT NULL,
  status TEXT NOT NULL,             -- running | done | failed | timeout
  trigger_node TEXT, started_at INTEGER, ended_at INTEGER, hops INTEGER
);

CREATE TABLE run_hops (              -- chỉ ghi khi debug
  id INTEGER PRIMARY KEY, run_id INTEGER NOT NULL, seq INTEGER,
  node TEXT, rule TEXT, in_port TEXT, out_port TEXT,
  kind TEXT, data TEXT, error TEXT, ts INTEGER, dur_ms INTEGER
);

CREATE TABLE node_state (            -- thay Redis
  chain_id INTEGER, node TEXT, scope TEXT, value TEXT, updated_at INTEGER,
  PRIMARY KEY (chain_id, node, scope)
);

CREATE TABLE logs (                  -- thay log_rule_engine_runners
  id INTEGER PRIMARY KEY, chain_id INTEGER, run_id INTEGER,
  level TEXT, node TEXT, message TEXT, ts INTEGER
);
```

Ba khác biệt có chủ đích so với schema Dipper:
`connect_to` (mảng chuỗi JSON, lặp 2 lần) → bảng `edges`;
`option.position` → cột `x`,`y`;
`log_rule_engine_runners` (chỉ có chan_id) → `logs` + `run_hops` có `run_id`.

### 6.1 Importer từ dữ liệu cũ

`POST /api/chains/import` nhận export JSON của `rule_chains` + `rule_nodes`:

1. Parse `connect_to[]` (mảng chuỗi JSON), **dedupe theo `id` edge** (đang lặp 2 lần).
2. `sourceHandle` null → `out`; `yes`/`no` → `true`/`false`.
3. Với `switch`: đọc `option.list_switch` (UI lưu) để dựng `cases`, `sourceHandle` chính
   là giá trị case.
4. Bóc `option.position` → cột `x`,`y`; bóc mọi `next_*`/`map_switch` nếu có (không nên có,
   vì chúng được inject lúc chạy).
5. Đổi `http_request` → `http-request`; cảnh báo node `delay`, `output-alert`,
   `output-kafka`, `create-model` (UI có, backend không hoặc lỗi).
6. Validate đồ thị (§6.2), trả về danh sách cảnh báo thay vì fail cứng.

### 6.2 Validate đồ thị (mới hoàn toàn)

Chạy khi lưu và khi activate: cổng tồn tại trong `RuleSpec`; `arity: One` không có 2 edge
vào; input port `All` phải có ≥2 edge; node source không có input; phát hiện node cô lập;
phát hiện chu trình (**cảnh báo**, không cấm — vòng lặp là hợp lệ nếu có `delay`/điều kiện
thoát); `config` khớp `config_schema`.

---

## 7. Ánh xạ phụ thuộc Go → SenClaw

| Gốc | Đích |
|---|---|
| Redis (state kalman/MA) | bảng `node_state` + cache in-memory |
| Redis Pub/Sub control channel `dipper:rule:control` | REST `/api/chains/{id}/reload` + `tokio::broadcast` nội bộ |
| Hub (Kafka/Redis/gRPC) | `tokio::broadcast` nội bộ; ingress ngoài qua HTTP webhook / MQTT tuỳ chọn |
| Postgres/SQLite của dipper-hub | SQLite riêng của app |
| `govaluate` + `strlen`/`sFromObj`/`nFromObj` | §7.1 |
| `core/daq` (reflection) | §7.2 |
| `sonyflake` | `sonyflake-rs` hoặc snowflake tự viết |
| Go plugin `.so` | bỏ — compile-in toàn bộ rule (engine-runner cũng đang compile-in) |

### 7.1 Biểu thức

Cần: `+ - * /`, so sánh, `&&`/`||`, ternary, `!=` **và** `<>`, `strlen(s)`,
`sFromObj(obj, path)`, `nFromObj(obj, path)`, ép kiểu chuỗi↔số ngầm định.

Đề xuất: **`evalexpr`** + shim (alias `<>`, đăng ký 3 hàm custom, chuẩn hoá mọi số về
`f64`). `rhai` quá nặng và là ngôn ngữ script; `cel-rust` cú pháp lệch xa hơn.

**Test oracle**: port nguyên bảng test `pkg/rules/arithmetic/math_test.go:9-99`
(`nFromObj(ac,'a')+b`, `a-b`, `a*b`, `a+10`, `(a+b)*(a+x)`) làm bộ so khớp bắt buộc.

### 7.2 DAQ (path truy vấn dữ liệu)

Viết lại sạch trên `serde_json::Value`: path `a.b.c`, index `arr[0]`. **Sửa** các quirk của
bản gốc: `float64` bị phân loại nhầm thành String (`toType` dùng `CanInt`);
`Query.Number()` cache sai với giá trị `0`; `Query.String()` cache sai với chuỗi rỗng;
`Query.Array()` có logic đảo ngược khiến **mọi truy cập mảng đều lỗi**.

Giữ nguyên `DataToValue` / `ValueToData` (flatten branch `default` lên top-level, gói data
gốc vào biến `meta_data`) vì biểu thức người dùng đang dựa vào đó.

### 7.3 Nhóm node IoT — cần quyết định

`input-telemetry`, `input-model`, `input-stream`, `output-model`, `output-action`,
`get-last-model` gắn chặt vào domain Dipper (bảng device/model/namespace + hub topic).
SenClaw không có các bảng đó. Ba lựa chọn — xem §10 câu hỏi 1.

### 7.4 App→app MCP

`mcp.call` của bridge là stub, nên node `mcp-call` sẽ POST thẳng
`{origin}/api/mcp/message` của app đích, với origin lấy từ `GET /api/space/apps`
(manifest `runtime.url`, cache TTL 20s) — đúng pattern `apps/search/src/transport/app_mcp.rs`.

### 7.5 `ai-agent` — cần quyết định

Gốc: cấu hình provider + api_key ngay trong node (chatgpt/deepseek/ollama/gemini/dify),
gọi HTTP trực tiếp. SenClaw có bridge `llm.request` (không temperature, `finish=="length"`
là lỗi) và `agent.run` (có persona + tool allowlist + timeout 10..1800s). Xem §10 câu hỏi 2.

---

## 8. UI

### 8.1 Thư viện

Gốc dùng `reactflow` v11 + React 18. Space App chuẩn là **React 19 + AntD 6 + Vite 8** →
phải dùng **`@xyflow/react` v12** (v11 không hỗ trợ React 19). API tương đương, đổi import
và một số tên props.

### 8.2 Node render tự sinh từ registry

Gốc: 24 file TSX hard-code `<Handle>` cho từng rule, `top: 25 + 10*index` (các port dính
nhau khi >3 case), không có label cạnh handle, 4 node thiếu `id` handle
(`sourceHandle`/`targetHandle` = null).

Mới: **một** component `RuleNode` generic đọc `RuleSpec` từ `GET /api/registry`, tự sinh
handle với label, khoảng cách 22px, màu theo `PortSpec.color`. Form config sinh từ
`config_schema` (JSON Schema) với override thủ công cho vài node phức tạp
(`arithmetic` bảng operator, `switch` bảng case, `output-model` bảng field mapping).

Thêm những thứ gốc không có: kéo-thả từ palette (có sẵn implementation tham chiếu ở
`frontend/src/modules/scada/components/scada-canvas-edit.tsx:179-192`), `isValidConnection`
(chặn nối sai cổng / sai arity), auto-layout, undo/redo, copy-paste, node mới không còn
luôn rơi vào `{x:0, y:50}`.

### 8.3 Layout

Giữ đúng bố cục gốc: canvas full-height + `MiniMap`/`Controls`/`Background`, ToolsBox nổi
bên phải (create / save / debug / logs / back), Drawer 600px cấu hình node với 3 tab
(Option / Link / Document), Drawer log ở dưới cao 70%.
Bỏ tab "Document" hard-code `'# Hi, *Pluto*!'` — thay bằng doc lấy từ `RuleSpec`.

### 8.4 Debug console realtime

Gốc: `debug-console.tsx` là **stub rỗng** (code bị comment), log chỉ lấy được bằng cách
bấm Refresh gọi GraphQL query.

Mới: `GET /api/runs/stream` (SSE) đẩy `run_hops` realtime; canvas highlight node/edge đang
chảy; click hop xem data trước/sau. Đây là thứ khiến engine dùng được thật.

### 8.5 Không port `rule_builder/`

`frontend/src/modules/rule_builder/` là **rule engine thứ hai**, dành cho firmware ESP32
(enum số, action phẳng, lưu 1 chuỗi `rulesJson`, converter 2 chiều UI↔firmware). Không
liên quan `rule_chan`, khác API, khác paradigm. Ngoài phạm vi app này.

---

## 9. Kế hoạch triển khai

| Phase | Nội dung | Ước lượng |
|---|---|---|
| 0 | Scaffold: Cargo + workspace member, manifest (port 4550), main.rs (5 candidate dist path), config, db, `/api/status`, MCP skeleton, pack.sh | ~600 LOC |
| 1 | Engine core: types, graph load/validate, scheduler per-node, router theo edge, run lifecycle + reaper, cổng `error` ngầm định | ~1.8k LOC |
| 2 | **Port model**: RuleSpec registry, dynamic ports, join buffer + 3 policy + TTL, node `join`/`merge`/`split`, deep-clone tại fan-out | ~1.2k LOC |
| 3 | `expr/` (evalexpr shim + oracle test) + `daq` sạch + rule pack thuần: arithmetic, conditional, switch, fork, format, project, trigger_time, log, delay | ~2.2k LOC |
| 4 | Rule pack I/O: http-request, webhook ingress, schedule (cron), telegram send/hook, moving_average + kalman (node_state), notification | ~1.8k LOC |
| 5 | Pack SenClaw-native: ai-agent (bridge), ai-persona (`agent.run`), knowledge, mcp-call, senclaw-send | ~1.2k LOC |
| 6 | UI: canvas @xyflow/react 12, node tự sinh từ registry, palette kéo-thả, form từ schema, SSE debug console, danh sách chain | ~3.5k LOC TS |
| 7 | MCP tools `rule_*` + skills + personas + importer dữ liệu cũ + README | ~1.2k LOC |
| 8 | Nhóm IoT (tuỳ quyết định §10.1) | ~800 LOC |

Tổng ~10k LOC Rust + ~3.5k LOC TS. Mỗi phase có test co-located (`#[cfg(test)]`), gồm
drift-guard MCP theo pattern `apps/search/src/mcp.rs:521-544`.

### 9.1 MCP tools dự kiến

`rule_list_chains`, `rule_get_chain`, `rule_create_chain`, `rule_update_graph`,
`rule_delete_chain`, `rule_activate`, `rule_deactivate`, `rule_registry` (liệt kê rule +
cổng + schema), `rule_validate`, `rule_trigger` (bơm event thủ công vào node `manual`),
`rule_runs`, `rule_run_trace`, `rule_logs`, `rule_node_state`, `rule_import`, `rule_export`,
`rule_generate` (AI dựng chain từ mô tả tiếng Việt).

---

## 10. Quyết định đã chốt (2026-07-20)

**1. Nhóm node IoT** → **generic core + gói `dipper` tuỳ chọn.** Engine và toàn bộ rule
pack 1-5 không biết gì về domain IoT. Sáu node `input-telemetry`, `input-model`,
`input-stream`, `output-model`, `output-action`, `get-last-model` gom vào một *rule pack*
riêng (`src/rules/dipper/`) đăng ký có điều kiện, backend quyết định sau (`apps/hub` MCP
hay Postgres trực tiếp). Phase 8, không chặn phase 0-7.

**2. `ai-agent`** → **hỗ trợ cả hai.** `config.backend`:
   - `"senclaw"` (mặc định) → bridge `llm.request`; `finish == "length"` là lỗi → cổng `error`.
   - `"persona"` → bridge `agent.run` với `persona` + `tools` allowlist + `timeoutSeconds`.
   - `"provider"` → HTTP trực tiếp: chatgpt / deepseek / ollama / gemini / dify, giữ đúng
     schema option của bản gốc (`provider`, `model`, `api_key`, `host`, `base_url`,
     `system_prompt`, `user_prompt`).

**3. Dữ liệu cũ** → **không có chain nào cần import.** Bỏ `POST /api/chains/import` và
mục §6.1 khỏi phạm vi. Schema mới sạch, không cần đọc `connect_to`.

**4. Tương thích ngược** → **không cần.** Không chạy song song với engine Go.
