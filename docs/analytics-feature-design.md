# Thiết kế tính năng Analytics cho SenClaw

> Tài liệu thiết kế kỹ thuật — tháng 5/2026

---

## Mục lục

1. [Tổng quan](#1-tổng-quan)
2. [Kiến trúc thu thập dữ liệu](#2-kiến-trúc-thu-thập-dữ-liệu)
3. [Schema SQLite](#3-schema-sqlite)
4. [MCP Analytics Server](#4-mcp-analytics-server)
5. [Web UI Dashboard](#5-web-ui-dashboard)
6. [Tích hợp vào SenClaw](#6-tích-hợp-vào-senclaw)
7. [Kết luận](#7-kết-luận)

---

## 1. Tổng quan

### 1.1 Tại sao Analytics quan trọng với personal AI agent?

SenClaw là hệ thống agent chạy liên tục — xử lý tin nhắn, gọi tool, tốn token, tốn tiền. Không có analytics, người dùng không biết:

- Agent nào đang "ngốn" token nhiều nhất?
- Tool nào hay fail, làm chậm response?
- Chi phí thực tế mỗi ngày/tuần là bao nhiêu?
- Giờ nào trong ngày agent được dùng nhiều — để tối ưu caching hay schedule?

Analytics không phải "nice to have" — nó là công cụ điều hành hệ thống agent.

### 1.2 Phạm vi tính năng

| Nhóm metric | Nội dung |
|---|---|
| **Agent usage** | Conversations, messages, token in/out, response time, error rate |
| **Tool analytics** | Tool call count, success/fail, avg execution time, top tools |
| **Performance** | LLM latency (p50/p95/p99), queue wait time, concurrency |
| **User behavior** | Active hours heatmap, query volume by day, topic tags |
| **Cost tracking** | Token cost per model, per group/agent, daily/weekly total |

---

## 2. Kiến trúc thu thập dữ liệu

### 2.1 Event pipeline

```
┌─────────────────────────────────────────────────────────────┐
│                   Instrumentation Points                     │
│                                                             │
│  MessageRouter ──► AgentPool ──► ZenCore ──► MCP tools     │
│       │               │             │            │          │
│       ▼               ▼             ▼            ▼          │
│  msg_received    session_start  llm_request  tool_call      │
│  msg_processed   session_end   llm_response tool_result     │
└──────────────────────────┬──────────────────────────────────┘
                           │  AnalyticsCollector (in-process)
                           ▼
                  ┌────────────────┐
                  │  Event Buffer  │  (tokio channel, 10k cap)
                  │  (async, MPSC) │
                  └───────┬────────┘
                          │  flush every 5s or 100 events
                          ▼
                  ┌────────────────┐
                  │  SQLite DB     │  analytics_events + agg tables
                  │  (WAL mode)    │
                  └────────────────┘
```

### 2.2 Event schema (Rust)

```rust
// src/analytics/types.rs

#[derive(Debug, Clone, Serialize)]
pub enum AnalyticsEvent {
    MessageReceived {
        group_jid: String,
        channel: String,           // "telegram" | "feishu" | "web"
        ts: i64,
    },
    LlmRequest {
        group_jid: String,
        agent_id: String,
        model: String,             // "claude-sonnet-4-6" | "gpt-4o" | ...
        prompt_tokens: u32,
        ts: i64,
    },
    LlmResponse {
        group_jid: String,
        agent_id: String,
        model: String,
        completion_tokens: u32,
        latency_ms: u64,
        success: bool,
        ts: i64,
    },
    ToolCall {
        group_jid: String,
        tool_name: String,         // "Bash" | "mcp__browser__search" | ...
        ts: i64,
    },
    ToolResult {
        group_jid: String,
        tool_name: String,
        success: bool,
        duration_ms: u64,
        ts: i64,
    },
    SessionStart { group_jid: String, ts: i64 },
    SessionEnd   { group_jid: String, turns: u32, ts: i64 },
}
```

### 2.3 AnalyticsCollector

```rust
// src/analytics/collector.rs

pub struct AnalyticsCollector {
    tx: tokio::sync::mpsc::Sender<AnalyticsEvent>,
}

impl AnalyticsCollector {
    pub fn new(db: Arc<Db>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10_000);
        tokio::spawn(async move {
            let mut buf: Vec<AnalyticsEvent> = Vec::with_capacity(100);
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    Some(ev) = rx.recv() => {
                        buf.push(ev);
                        if buf.len() >= 100 { flush(&db, &mut buf); }
                    }
                    _ = interval.tick() => {
                        if !buf.is_empty() { flush(&db, &mut buf); }
                    }
                }
            }
        });
        Self { tx }
    }

    pub fn emit(&self, event: AnalyticsEvent) {
        let _ = self.tx.try_send(event); // non-blocking, drop on overflow
    }
}
```

### 2.4 Điểm đặt instrument

| File | Hook | Event |
|---|---|---|
| `src/gateway/message_router.rs` | Sau khi nhận message | `MessageReceived` |
| `src/agent/agent_pool/pool.rs` | `bind_events` → `on_process_*` | `SessionStart/End` |
| `src/zen_core/engine.rs` | Trước/sau gọi LLM | `LlmRequest/Response` |
| `src/zen_core/run_tools.rs` | Wrap tool execution | `ToolCall/Result` |

---

## 3. Schema SQLite

### 3.1 Raw events table

```sql
-- Lưu raw events, giữ 90 ngày
CREATE TABLE analytics_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT    NOT NULL,
    group_jid  TEXT,
    agent_id   TEXT,
    tool_name  TEXT,
    model      TEXT,
    channel    TEXT,
    prompt_tokens     INTEGER DEFAULT 0,
    completion_tokens INTEGER DEFAULT 0,
    latency_ms        INTEGER DEFAULT 0,
    duration_ms       INTEGER DEFAULT 0,
    success    INTEGER DEFAULT 1,   -- 0/1 boolean
    ts         INTEGER NOT NULL     -- Unix ms
);

CREATE INDEX idx_ae_ts       ON analytics_events(ts);
CREATE INDEX idx_ae_group    ON analytics_events(group_jid, ts);
CREATE INDEX idx_ae_type     ON analytics_events(event_type, ts);
CREATE INDEX idx_ae_tool     ON analytics_events(tool_name, ts);
```

### 3.2 Aggregation tables (pre-computed, updated mỗi giờ)

```sql
-- Tổng hợp theo ngày x group
CREATE TABLE analytics_daily (
    date       TEXT NOT NULL,         -- "2026-05-06"
    group_jid  TEXT NOT NULL,
    messages   INTEGER DEFAULT 0,
    sessions   INTEGER DEFAULT 0,
    prompt_tokens     INTEGER DEFAULT 0,
    completion_tokens INTEGER DEFAULT 0,
    tool_calls INTEGER DEFAULT 0,
    tool_errors INTEGER DEFAULT 0,
    est_cost_usd REAL DEFAULT 0.0,
    PRIMARY KEY (date, group_jid)
);

-- Tool stats theo ngày
CREATE TABLE analytics_tools_daily (
    date       TEXT NOT NULL,
    tool_name  TEXT NOT NULL,
    call_count INTEGER DEFAULT 0,
    error_count INTEGER DEFAULT 0,
    avg_duration_ms REAL DEFAULT 0.0,
    PRIMARY KEY (date, tool_name)
);

-- LLM latency percentiles theo giờ
CREATE TABLE analytics_latency_hourly (
    hour       TEXT NOT NULL,         -- "2026-05-06T14"
    model      TEXT NOT NULL,
    p50_ms     INTEGER DEFAULT 0,
    p95_ms     INTEGER DEFAULT 0,
    p99_ms     INTEGER DEFAULT 0,
    sample_count INTEGER DEFAULT 0,
    PRIMARY KEY (hour, model)
);
```

### 3.3 Model pricing table

```sql
CREATE TABLE model_pricing (
    model          TEXT PRIMARY KEY,
    input_per_1m   REAL NOT NULL,  -- USD per 1M tokens
    output_per_1m  REAL NOT NULL
);

INSERT INTO model_pricing VALUES
    ('claude-sonnet-4-6',     3.0,  15.0),
    ('claude-haiku-4-5',      0.8,   4.0),
    ('claude-opus-4-7',      15.0,  75.0),
    ('gpt-4o',                5.0,  15.0),
    ('gpt-4o-mini',           0.15,  0.6);
```

---

## 4. MCP Analytics Server

### 4.1 Server `senclaw-analytics`

Agent có thể hỏi analytics bằng ngôn ngữ tự nhiên thông qua MCP tools:

```rust
// src/mcp/analytics_server.rs

#[derive(rmcp::tool)]
struct McpAnalyticsServer { db: Arc<Db> }

impl McpAnalyticsServer {
    /// Lấy tổng hợp analytics theo khoảng thời gian.
    /// period: "today" | "yesterday" | "this_week" | "this_month" | "last_7d" | "last_30d"
    #[tool]
    async fn analytics_summary(&self, period: String) -> Result<String> {
        let (from, to) = resolve_period(&period)?;
        let rows = self.db.query_analytics_daily(from, to)?;
        // Format thành text summary cho LLM đọc
        Ok(format_summary(rows))
    }

    /// Truy vấn metric cụ thể.
    /// metric: "messages" | "tokens" | "cost" | "tool_calls" | "latency"
    #[tool]
    async fn analytics_query(
        &self,
        metric: String,
        period: String,
        group_by: Option<String>,   // "agent" | "day" | "tool"
    ) -> Result<serde_json::Value> { ... }

    /// Dữ liệu cho chart (time-series).
    #[tool]
    async fn analytics_chart(
        &self,
        metric: String,   // "messages_per_day" | "cost_per_day" | "tool_usage"
        period: String,
    ) -> Result<serde_json::Value> { ... }

    /// Top tools theo tần suất sử dụng.
    #[tool]
    async fn analytics_top_tools(&self, period: String, limit: Option<u32>) -> Result<String> { ... }

    /// Chi phí ước tính.
    #[tool]
    async fn analytics_cost_breakdown(
        &self,
        period: String,
        group_by: Option<String>,  // "model" | "agent" | "day"
    ) -> Result<serde_json::Value> { ... }
}
```

### 4.2 Ví dụ agent query

```
User: "Tuần này tôi đã tốn bao nhiêu tiền dùng AI?"

Agent calls: analytics_cost_breakdown(period="this_week", group_by="model")
→ {
    "total_usd": 2.34,
    "breakdown": [
      { "model": "claude-sonnet-4-6", "usd": 1.89, "tokens": 126000 },
      { "model": "claude-haiku-4-5",  "usd": 0.45, "tokens": 450000 }
    ]
  }

Agent: "Tuần này bạn đã dùng $2.34 — chủ yếu từ Sonnet ($1.89) cho
        các tác vụ phức tạp và Haiku ($0.45) cho tác vụ nhanh."
```

---

## 5. Web UI Dashboard

### 5.1 Layout tổng thể

```
AnalyticsDashboard
├── PeriodSelector          (Today / 7d / 30d / Custom range)
├── OverviewCards           (4 cards ngang)
│   ├── TotalMessages
│   ├── TotalTokens
│   ├── EstimatedCost
│   └── ActiveGroups
├── ChartsRow
│   ├── ActivityChart       (line chart: messages + sessions per day)
│   └── CostChart           (bar chart: cost per day, stacked by model)
├── ToolsSection
│   ├── TopToolsTable       (tool name, calls, errors, avg duration)
│   └── ToolHeatmap         (tool × hour-of-day matrix)
├── LatencySection
│   └── LatencyChart        (p50/p95/p99 per model, last 24h)
└── CostBreakdown
    └── ModelPieChart       (cost share per model)
```

### 5.2 Components chính

```tsx
// web/src/pages/AnalyticsPage.tsx
export function AnalyticsPage() {
  const { period, setPeriod } = usePeriodState();
  const { data, loading } = useAnalytics(period);
  return (
    <div className="p-6 space-y-6">
      <PeriodSelector value={period} onChange={setPeriod} />
      <OverviewCards data={data?.overview} loading={loading} />
      <div className="grid grid-cols-2 gap-4">
        <ActivityChart data={data?.activity} />
        <CostChart data={data?.cost} />
      </div>
      <TopToolsTable data={data?.tools} />
      <ToolHeatmap data={data?.heatmap} />
      <div className="grid grid-cols-2 gap-4">
        <LatencyChart data={data?.latency} />
        <ModelPieChart data={data?.costByModel} />
      </div>
    </div>
  );
}
```

```tsx
// OverviewCards — 4 số tổng hợp nhanh
function OverviewCards({ data, loading }) {
  const cards = [
    { title: 'Tin nhắn',    value: data?.total_messages,      suffix: 'msgs',  icon: <MessageOutlined /> },
    { title: 'Tokens',      value: fmtTokens(data?.total_tokens), suffix: '',  icon: <ThunderboltOutlined /> },
    { title: 'Chi phí',     value: `$${data?.cost_usd?.toFixed(2)}`, suffix: '', icon: <DollarOutlined /> },
    { title: 'Agent active',value: data?.active_groups,        suffix: '',      icon: <RobotOutlined /> },
  ];
  return (
    <div className="grid grid-cols-4 gap-4">
      {cards.map(c => (
        <Card key={c.title} loading={loading}>
          <Statistic title={c.title} value={c.value} suffix={c.suffix} prefix={c.icon} />
        </Card>
      ))}
    </div>
  );
}
```

```tsx
// ActivityChart — dùng recharts LineChart
import { LineChart, Line, XAxis, YAxis, Tooltip, Legend } from 'recharts';

function ActivityChart({ data }) {
  return (
    <Card title="Hoạt động theo ngày">
      <LineChart width={500} height={250} data={data}>
        <XAxis dataKey="date" />
        <YAxis />
        <Tooltip />
        <Legend />
        <Line type="monotone" dataKey="messages" stroke="#6366f1" name="Tin nhắn" />
        <Line type="monotone" dataKey="sessions"  stroke="#10b981" name="Sessions" />
      </LineChart>
    </Card>
  );
}
```

```tsx
// ToolHeatmap — ma trận tool × giờ trong ngày
// Dùng antd Table với cell coloring dựa trên count
function ToolHeatmap({ data }) {
  // data: { tool: string, hours: number[24] }[]
  const maxVal = Math.max(...data.flatMap(r => r.hours));
  const columns = [
    { title: 'Tool', dataIndex: 'tool', fixed: 'left' },
    ...Array.from({length: 24}, (_, h) => ({
      title: `${h}h`,
      dataIndex: ['hours', h],
      width: 36,
      render: (v) => (
        <div
          className="text-center text-xs rounded"
          style={{ background: heatColor(v, maxVal), padding: '2px 0' }}
        >
          {v || ''}
        </div>
      ),
    }))
  ];
  return <Table dataSource={data} columns={columns} scroll={{ x: true }} pagination={false} />;
}
```

### 5.3 REST API endpoints

```
GET /api/analytics/overview?period=7d
GET /api/analytics/activity?period=30d
GET /api/analytics/tools?period=7d&limit=20
GET /api/analytics/latency?hours=24
GET /api/analytics/cost?period=30d&group_by=model
GET /api/analytics/heatmap?period=7d    # tool × hour matrix
```

### 5.4 Hook `useAnalytics`

```tsx
function useAnalytics(period: string) {
  const [data, setData] = useState(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    setLoading(true);
    Promise.all([
      fetch(`/api/analytics/overview?period=${period}`).then(r => r.json()),
      fetch(`/api/analytics/activity?period=${period}`).then(r => r.json()),
      fetch(`/api/analytics/tools?period=${period}`).then(r => r.json()),
      fetch(`/api/analytics/cost?period=${period}&group_by=model`).then(r => r.json()),
      fetch(`/api/analytics/heatmap?period=${period}`).then(r => r.json()),
    ]).then(([overview, activity, tools, costByModel, heatmap]) => {
      setData({ overview, activity, tools, costByModel, heatmap });
      setLoading(false);
    });
  }, [period]);

  return { data, loading };
}
```

---

## 6. Tích hợp vào SenClaw

### 6.1 Files mới cần tạo

```
src/
├── analytics/
│   ├── mod.rs           # pub use collector, types, query
│   ├── types.rs         # AnalyticsEvent enum
│   ├── collector.rs     # AnalyticsCollector (MPSC buffer → DB)
│   ├── query.rs         # Hàm query DB: daily, tools, latency
│   └── aggregator.rs    # Background task cập nhật agg tables mỗi giờ
├── mcp/
│   └── analytics_server.rs   # McpAnalyticsServer (senclaw-analytics)
└── gateway/
    └── ui_server/
        └── analytics.rs  # REST handlers cho /api/analytics/*

web/src/
├── pages/AnalyticsPage.tsx
├── components/analytics/
│   ├── OverviewCards.tsx
│   ├── ActivityChart.tsx
│   ├── CostChart.tsx
│   ├── TopToolsTable.tsx
│   ├── ToolHeatmap.tsx
│   ├── LatencyChart.tsx
│   └── ModelPieChart.tsx
└── hooks/useAnalytics.ts
```

### 6.2 DB migration

Thêm vào `src/db/schema.rs`:
```rust
pub const ANALYTICS_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS analytics_events ( ... );
    CREATE TABLE IF NOT EXISTS analytics_daily ( ... );
    CREATE TABLE IF NOT EXISTS analytics_tools_daily ( ... );
    CREATE TABLE IF NOT EXISTS analytics_latency_hourly ( ... );
    CREATE TABLE IF NOT EXISTS model_pricing ( ... );
";
```

### 6.3 Wiring trong `src/lib.rs`

```rust
// Khởi tạo AnalyticsCollector, truyền vào AgentPool + ZenCore
let analytics = Arc::new(AnalyticsCollector::new(Arc::clone(&db)));
agent_pool.set_analytics(Arc::clone(&analytics));

// Background aggregator chạy mỗi giờ
tokio::spawn(analytics::aggregator::run(Arc::clone(&db)));

// Register MCP server
mcp_manager.register_builtin("senclaw-analytics", analytics_server_config(&db_path));
```

### 6.4 Instrument hooks trong AgentPool

```rust
// pool.rs — sau khi LLM trả về
self.analytics.emit(AnalyticsEvent::LlmResponse {
    group_jid: jid.clone(),
    agent_id:  binding.agent_id.clone(),
    model:     model_name.clone(),
    completion_tokens: response.usage.output_tokens,
    latency_ms: start.elapsed().as_millis() as u64,
    success: true,
    ts: now_ms(),
});
```

### 6.5 Route mới trong `core.rs`

```rust
.route("/api/analytics/overview",  get(analytics_overview))
.route("/api/analytics/activity",  get(analytics_activity))
.route("/api/analytics/tools",     get(analytics_tools))
.route("/api/analytics/cost",      get(analytics_cost))
.route("/api/analytics/latency",   get(analytics_latency))
.route("/api/analytics/heatmap",   get(analytics_heatmap))
```

### 6.6 Navigation

Thêm tab "Analytics" vào sidebar của Web UI (giữa "Settings" và "MCP Plugins"), icon `BarChartOutlined`.

---

## 6b. Analytics cho Code Feature & Knowledge Graph

### 6b.1 Bổ sung events cho Code tools

Khi tính năng Code và Knowledge Graph được triển khai, cần thêm các events sau vào `AnalyticsEvent`:

```rust
// Bổ sung vào src/analytics/types.rs
AnalyticsEvent::CodeToolCall {
    group_jid: String,
    tool: String,           // "read_file" | "edit_file" | "write_file" | "bash"
    file_path: Option<String>,
    lines_affected: Option<u32>,
    duration_ms: u64,
    success: bool,
    ts: i64,
},
AnalyticsEvent::GraphIndexed {
    project_id: String,
    files_indexed: u32,
    symbols_found: u32,
    relations_found: u32,
    duration_ms: u64,
    ts: i64,
},
AnalyticsEvent::GraphQuery {
    project_id: String,
    query_type: String,     // "callers_of" | "impact_analysis" | "trace_flow"
    result_count: u32,
    duration_ms: u64,
    ts: i64,
},
```

### 6b.2 Dashboard bổ sung: Code Analytics tab

```
CodeAnalyticsSection (trong AnalyticsDashboard)
├── CodeActivityCard      — sessions, edits/day, files touched
├── TopEditedFilesTable   — file path, edit count, last edited
├── ToolUsageByCategory   — pie chart: read vs edit vs bash vs graph
└── GraphIndexStats       — symbols indexed, relations, last reindex time
```

```tsx
// Thêm vào AnalyticsPage.tsx
{activeTab === 'code' && (
  <div className="space-y-4">
    <div className="grid grid-cols-3 gap-4">
      <Statistic title="Files edited (7d)"  value={data?.code?.files_edited} />
      <Statistic title="Code sessions"      value={data?.code?.sessions} />
      <Statistic title="Graph symbols"      value={fmtK(data?.code?.graph_symbols)} />
    </div>
    <TopEditedFilesTable data={data?.code?.top_files} />
    <ToolUsagePieChart    data={data?.code?.tool_breakdown} />
  </div>
)}
```

### 6b.3 API endpoint bổ sung

```
GET /api/analytics/code?period=7d
→ {
    sessions: 12,
    files_edited: 34,
    tool_breakdown: { read: 156, edit: 43, write: 12, bash: 89, graph: 28 },
    top_files: [{ path: "src/lib.rs", edits: 8 }, ...],
    graph_symbols: 4231,
    graph_relations: 12089,
    last_reindex: "2026-05-06T14:30:00Z"
  }
```

## 7. Kết luận

### 7.1 Thứ tự triển khai

**Phase 1 — Data collection (1 tuần)**
- [ ] Schema migration: `analytics_events` table
- [ ] `AnalyticsCollector` với MPSC buffer
- [ ] Instrument 4 điểm: message, session, LLM call, tool call
- [ ] Basic REST endpoint `/api/analytics/overview`

**Phase 2 — Dashboard UI (1 tuần)**
- [ ] `AnalyticsPage` với OverviewCards + ActivityChart
- [ ] CostChart + TopToolsTable
- [ ] `useAnalytics` hook
- [ ] Model pricing table + cost calculation

**Phase 3 — Agent integration (3-4 ngày)**
- [ ] Aggregation tables + background aggregator
- [ ] `senclaw-analytics` MCP server với 5 tools
- [ ] ToolHeatmap + LatencyChart
- [ ] Đăng ký vào `get_builtin_servers()`

### 7.2 Đề xuất kỹ thuật

1. **Non-blocking always** — dùng `try_send` (drop event nếu buffer đầy), không bao giờ block agent loop vì analytics.
2. **Retention policy** — xóa `analytics_events` cũ hơn 90 ngày bằng scheduled task hàng ngày. Agg tables giữ mãi (nhỏ).
3. **Privacy** — không lưu nội dung message vào analytics, chỉ lưu metadata (token count, latency, tool name).
4. **Cost estimation** — giữ model pricing trong DB để dễ cập nhật khi Anthropic/OpenAI đổi giá, không hardcode trong code.
5. **recharts vs antd charts** — dùng recharts vì lightweight hơn, customizable hơn antd Charts (ít dependency hơn).

---

*Phiên bản: 1.0 — tháng 5/2026*
