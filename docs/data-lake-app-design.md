# Lakehouse Space App — Data Lake + Data Warehouse (thiết kế)

> Trạng thái: **nghiên cứu / đề xuất thiết kế**, chưa viết code.
> Ngày: 2026-07-21 (rev 2 — sau vòng review đối kháng 4 lăng kính, 36 finding đã xử lý)
> App: `apps/lakehouse` · Port **4560** · MCP `lakehouse-mcp` (prefix tool `lake_*`)
> Nghiên cứu nền: 8 agent song song — conventions nội bộ (apps/search, apps/json,
> apps/ontology, apps/rewrite-story, src/gateway/ui_server/space*.rs, root Cargo.toml)
> + khảo sát web (DataFusion/Arrow/Parquet, duckdb-rs, connector DB Rust, thiết kế ETL
> của dlt/Airbyte/SQLMesh/dbt/OpenLineage). Mọi version crate xác minh trên crates.io
> ngày 2026-07-21; các claim về repo xác minh tới file:line.

---

## 1. Mục tiêu & phạm vi

Một Space App biến SenClaw thành **data lake + data warehouse cá nhân**, theo 5 yêu cầu:

1. **Parquet là định dạng chuẩn của lake** — import CSV/JSON/Excel/Parquet thành dataset
   Parquet (hive-partition tùy chọn); query SQL trực tiếp; export ngược ra file/DB.
2. **Kết nối nhiều database** — Postgres, MySQL/MariaDB, SQLite là tier 1; SQL Server,
   ClickHouse, MongoDB, Snowflake, BigQuery, Oracle, ODBC là tier mở rộng (feature-gate).
3. **ETL** — extract từ nguồn → land Parquet với 4 sync mode chuẩn ngành:
   `full_refresh`, `incremental_append`, `incremental_merge`, `snapshot` (SCD2).
4. **Transform** — SQL (DataFusion) trên dataset đã land: `full` (rebuild) hoặc
   `incremental_by_time` (idempotent theo interval, có lookback + backfill).
5. **Flow dữ liệu chuyển đổi** — pipeline khai báo JSON/YAML: `sources → transforms →
   exports`, DAG suy ra từ tham chiếu, chạy nền có queue/cancel/resume/watchdog,
   tự lập lịch.
6. **MCP sâu + skill vận hành** — ~25 tool `lake_*` phủ toàn bộ vòng đời; 2 skill
   (analyst + operator) + persona data-engineer.

**Non-goal & ranh giới với app/module khác:**

- Không thay thế **knowledge/cognitive** (dữ liệu phi cấu trúc, semantic recall).
- **apps/json**: chuyển đổi format stateless một-lần (JSON↔CSV/XML/YAML) vẫn ở đó;
  lakehouse import luôn tạo **dataset Parquet có catalog** — khác mục đích.
- **apps/ontology**: sniffer `ingest.rs` được **copy** (không share crate — ontology land
  RDF, lakehouse land Parquet; hai vòng đời release độc lập).
- **apps/search**: tích hợp lakehouse làm search-source = ngoài scope v1.
- **apps/crm** (`crm_query`): DB nghiệp vụ riêng của CRM, không phải lake consumer v1.
- Không streaming/CDC realtime v1 (cursor-based là đủ; ghi rõ giới hạn *không phát hiện
  delete ở nguồn*). Không multi-process writer (single daemon; nhưng **trong** process
  vẫn phải per-flow exclusion + reader isolation — xem §6.5/§7).

---

## 2. Quyết định công nghệ

### 2.1 Query engine: **Apache DataFusion 54** (không phải DuckDB)

| Tiêu chí | DataFusion 54.0.0 | duckdb-rs 1.10504.0 (DuckDB 1.5.4) |
|---|---|---|
| Thuần Rust, static, không tải gì runtime | ✅ | ❌ C++ FFI; scanner postgres/mysql/sqlite + httpfs là **extension tải runtime** từ extensions.duckdb.org (offline = chết; static-link qua cargo chưa cover out-of-tree — PR #732 chỉ in-tree) |
| Async/axum | ✅ stream RecordBatch native | ❌ blocking, `Connection` Send-not-Sync → pool + spawn_blocking |
| SQL dialect | ANSI đủ dùng (window, CTE, subquery) | Rộng hơn (PIVOT, ASOF, MERGE INTO) |
| Hiệu năng scan Parquet | ~ngang (ClickBench đổi ngôi 2 lần 2024→2025) | Join nặng RAM ổn định hơn out-of-box |
| Build | cargo thuần; crate datafusion >100s compile | Bundled C++ nhiều phút/clean build — đụng CI cache chung + `lto=thin`/`codegen-units=1` |
| Ecosystem lock | arrow ^58.3 (dùng re-export) | arrow ^58; DuckDB v2.0 công bố fall 2026 (churn) |

**Chọn DataFusion** vì: (a) repo thiên hướng pure-Rust có chủ đích; (b) app là daemon
axum/tokio — engine async-native khớp kiến trúc; (c) **zero runtime download** bắt buộc
cho app cài offline; (d) mỗi Space App là binary riêng nên chi phí compile DataFusion
không lan sang daemon. DataFusion cover đủ surface warehouse: đọc/ghi Parquet
(hive-partition + pruning, `COPY TO`), `information_schema`, UDF các loại, streaming
execution + MemoryPool + spill-to-disk, và **`SQLOptions`/`sql_with_options`** để chặn
DDL/DML tận gốc plan (xem §7).

```toml
datafusion   = { version = "54", default-features = false, features = [
  "sql", "parquet", "compression", "datetime_expressions",
  "regex_expressions", "string_expressions", "unicode_expressions", "nested_expressions" ] }
object_store = { version = "0.13.2", features = ["fs"] }   # đúng pin của DF 54; KHÔNG lên 0.14
rusqlite     = { version = "0.32", features = ["bundled"] } # catalog — đúng pin workspace
# arrow/parquet: CHỈ dùng re-export datafusion::arrow / datafusion::parquet (58.3.x)
```

### 2.2 Storage model: **catalog-as-manifest** (Parquet thường + file-manifest trong SQLite)

Không dùng Delta/Iceberg:

- `deltalake` 0.32.4 write đầy đủ nhưng **pin datafusion ^53.1** — lệch 1 major.
- `iceberg-rust` 0.9.1: write chỉ append; UPDATE/DELETE/MERGE là epic mở (#2186).

Nhưng **cũng không dựa vào "atomic rename" thư mục** — review chỉ ra story đó sai ở 3
điểm chết người khi đường đọc là quét thư mục: (a) rename dir đè dir không rỗng fail
ENOTEMPTY, swap 2-rename không nguyên tử; (b) merge nhiều partition = N rename không
nguyên tử tập thể — reader thấy trạng thái xé đôi; (c) file đã rename mà SQLite txn chưa
commit thì query vẫn thấy → trùng dữ liệu vĩnh viễn sau crash. Đây chính là bài toán
manifest/snapshot mà Delta/Iceberg/DuckLake sinh ra để giải — và dlt từ chối merge trên
plain-parquet cũng vì vậy.

**Thiết kế chọn: manifest trong catalog** (đúng triết lý DuckLake — metadata trong SQL DB):

- Bảng `dataset_file` (§4) giữ **danh sách file active** của từng dataset. Query
  **không bao giờ tự quét thư mục**: `ListingTable` được dựng từ danh sách file tường
  minh + schema tường minh của catalog.
- **Ghi**: land file Parquet thẳng vào `lake/<ns>/<ds>/part-<run_id>-<seq>.parquet`
  (file nằm trên đĩa nhưng **vô hình** vì chưa vào manifest) → fsync → **MỘT transaction
  SQLite**: thêm file mới (active) + đánh dấu file bị thay thế (tombstone) + `step_run`
  + watermark + lineage + schema_version. Transaction đó **chính là swap** — nguyên tử
  với mọi mode, kể cả merge nhiều partition và full_refresh (không cần rename nào).
- **Crash-recovery (boot reconcile)**: run `running` mồ côi → `failed`; **file trên đĩa
  không có trong manifest** (mọi trạng thái) và thuộc run không-commit → xóa. Tên file
  chứa `run_id` nên quét được. Compaction có run row riêng nên output của nó sống sót.
- **GC**: file tombstone bị xóa vật lý sau grace period ≥ 2× `query_max_seconds`
  (setting, mặc định 600s) — reader snapshot danh sách file lúc plan nên grace period
  cho snapshot-isolation thực dụng (§7).
- `dataset.format` giữ giá trị `parquet` mặc định — chừa đường nâng từng dataset lên
  `delta` khi delta-rs bắt kịp DF major; `datafusion-ducklake` (contrib) là đường
  tương thích DuckLake nếu cần về sau.

### 2.3 Interchange nội bộ & quy tắc biên arrow

Mọi extract/transform/load đi qua **`Stream<RecordBatch>` arrow 58.3** (re-export DF 54).
Quy tắc biên (đã xác minh pin từng crate):

| Crate | Pin arrow | Cách nối |
|---|---|---|
| pgpq 0.11.1 | `>=56` **không chặn trên** → lockfile tươi resolve ra 59 (lệch 58.3!) | Pin lockfile `cargo update -p arrow-array --precise 58.3.0` cho edge của pgpq + **CI check chỉ có 1 version arrow-array trong Cargo.lock**; nếu vỡ, fallback Arrow IPC bytes tại biên |
| snowflake-api 0.14 | ^57 (+object_store 0.12) | Arrow IPC bytes tại biên (serialize bằng arrow-ipc 57 của nó, đọc lại bằng DF 58.3) |
| google-cloud-bigquery 0.15 | ^53 | Arrow IPC bytes tại biên |
| arrow-odbc 25.3 | theo sát arrow mới | Khớp major khi bật feature `odbc`, kiểm tra lúc nâng bộ DF |

Nâng version: DF + arrow + parquet + object_store (+ pgpq pin) là **MỘT bộ**, nâng cùng lúc.

---

## 3. Kiến trúc app

### 3.1 Cấu trúc thư mục (template apps/search + apps/json)

```
apps/lakehouse/
  Cargo.toml                  # thêm vào root [workspace].members
  senclaw-manifest.json       # id "lakehouse", port 4560, mcp "lakehouse-mcp"
  README.md                   # header port/MCP/dev-run + mục "Decisions" (chưng cất §2, §11)
  src/
    main.rs                   # axum boot; #![recursion_limit = "512"]
    config.rs                 # MỌI env đọc ở đây (PORT, SENCLAW_BASE_URL, LAKEHOUSE_DATA_DIR…)
    api.rs                    # REST (§8; path không prefix /api — main.rs nest)
    mcp.rs                    # JSON-RPC 2.0 hand-rolled: /api/mcp/sse + /api/mcp/message
    db.rs + schema.sql        # catalog SQLite (rusqlite 0.32, Mutex<Connection>, WAL)
    engine.rs                 # DataFusion: manifest→ListingTable, SQLOptions, MemoryPool
    lake.rs                   # ghi Parquet, manifest commit, GC, compaction, reconcile
    ingest.rs                 # sniff file (copy có ghi chú nguồn từ apps/ontology/src/ingest.rs)
    connectors/               # mod.rs (trait+ExtractSpec/LoadMode) postgres.rs mysql.rs sqlite.rs …
    flow.rs                   # DSL parse/validate, suy DAG, impact khi edit
    sync.rs                   # 4 sync mode + cursor + schema evolution
    runner.rs                 # queue + per-flow claim + cancel + watchdog + resume
    dashws.rs                 # WS hub {type,data,timestamp} (copy rewrite-story)
    transport/bridge.rs       # POST bridge llm.request (AI sinh flow/SQL)
  skills/lakehouse-analyst/SKILL.md
  skills/lakehouse-operator/SKILL.md
  personas/data-engineer.md
  scripts/pack.sh             # clone từ apps/hub
  web/                        # React + Vite + AntD
```

Checklist tích hợp repo: (1) thêm vào root `[workspace].members`; (2) claim port 4560
(4550 = rule-engine); (3) tùy chọn thêm `RUST_APPS` trong `.github/workflows/space-apps.yml`
(hiện 19/29 app ngoài CI — 9 Rust + 1 Node trong CI; pack local là chuẩn); (4) **không**
bật feature nào trên dep chung (serde_json `preserve_order`, chrono…) — feature
unification lan cả workspace.

### 3.2 senclaw-manifest.json

```json
{
  "id": "lakehouse",
  "name": "Lakehouse",
  "description": "Data lake + warehouse cá nhân: import file/DB thành dataset Parquet, query SQL (DataFusion), pipeline ETL sources→transforms→exports có lập lịch, incremental sync, backfill, lineage. Dùng lakehouse-mcp để agent tự vận hành.",
  "icon": "🏞️",
  "runtime": { "kind": "server", "start": "./lakehouse", "healthPath": "/api/status", "port": 4560 },
  "integration": { "type": "iframe", "url": "/" },
  "bridge": { "postMessage": true, "capabilities": ["space.rest", "llm.request"] },
  "mcp": { "name": "lakehouse-mcp", "transport": "http", "path": "/api/mcp/sse",
           "description": "<viết đầy đủ ở Phase 1 — xem deliverable §12>", "autoRegister": true },
  "skills": [
    { "name": "lakehouse-analyst",  "path": "skills/lakehouse-analyst",  "triggers": ["query dữ liệu", "phân tích dataset", "sql lakehouse", "xem dữ liệu parquet", "thống kê bảng"] },
    { "name": "lakehouse-operator", "path": "skills/lakehouse-operator", "triggers": ["etl", "đồng bộ database", "tạo pipeline", "import parquet", "backfill", "kết nối postgres", "data warehouse"] }
  ],
  "personas": [ { "name": "data-engineer", "path": "personas/data-engineer.md", "description": "Kỹ sư dữ liệu: thiết kế + vận hành pipeline trên lakehouse-mcp" } ]
}
```

Viết **đầy đủ** `mcp.description` (theo mật độ search-mcp: tóm tắt từng nhóm tool) và
trigger list là **deliverable Phase 1**, không phải placeholder khi ship.

### 3.3 Dữ liệu trên đĩa — NGOÀI thư mục cài đặt

Install zip `remove_dir_all` app dir (space.rs:957) → data ở ngoài (convention
rewrite-story):

```
~/.senclaw/space-app-data/lakehouse/        # override: LAKEHOUSE_DATA_DIR
  catalog.sqlite                            # catalog + manifest + flow + run + state (chmod 0600)
  lake/<namespace>/<dataset>/
    [key=value/]part-<run_id>-<seq>.parquet # file hiện diện ≠ file active — manifest quyết định
  inbox/                                    # thư mục cho lake_import_file{path} (allowlist)
  exports/                                  # file export đầy đủ cho user/agent tải
```

Giao thức ghi/đọc/GC: theo §2.2 (manifest là nguồn sự thật duy nhất; không staging-dir,
không rename; "swap" = 1 transaction SQLite).

---

## 4. Catalog (SQLite) — 10 bảng

Chưng cất từ dbt manifest/run_results + OpenLineage + Singer bookmark + DuckLake:

```sql
CREATE TABLE connection (
  id TEXT PRIMARY KEY, kind TEXT,               -- postgres|mysql|sqlite|mssql|clickhouse|…
  dsn TEXT,                                     -- lưu local; MCP/REST luôn redact
  created_at TEXT, last_ok_at TEXT);

CREATE TABLE dataset (
  id INTEGER PRIMARY KEY, namespace TEXT, name TEXT,
  format TEXT DEFAULT 'parquet',
  layer TEXT,                                   -- tag tự do (UI grouping, KHÔNG machinery)
  partition_cols TEXT,                          -- JSON ["date"] — bắt buộc cho merge/SCD2/incremental_by_time (§6.2)
  owner_flow_id TEXT,                           -- 1 dataset chỉ 1 flow ghi (NULL = import tay)
  current_schema_version INTEGER, row_count INTEGER, byte_size INTEGER,
  created_at TEXT, updated_at TEXT, UNIQUE(namespace, name));

CREATE TABLE dataset_file (                     -- MANIFEST — nguồn sự thật của mọi read (§2.2)
  id INTEGER PRIMARY KEY, dataset_id INTEGER, path TEXT,
  run_id TEXT, partition TEXT,                  -- JSON {"date":"2024-01-01"}
  row_count INTEGER, byte_size INTEGER,
  stats TEXT,                                   -- JSON min/max theo PK + time_column (prune merge/query)
  state TEXT CHECK(state IN ('active','tombstone')),
  created_at TEXT, tombstoned_at TEXT);
CREATE INDEX ix_file_dataset ON dataset_file(dataset_id, state);

CREATE TABLE schema_version (
  dataset_id INTEGER, version INTEGER,
  arrow_schema TEXT,                            -- JSON [{column_id,name,type,nullable}]
                                                -- column_id CHỈ để bookkeeping diff; parquet match theo TÊN (§6.4)
  change TEXT, created_at TEXT, PRIMARY KEY(dataset_id, version));

CREATE TABLE flow (
  id TEXT PRIMARY KEY, name TEXT,
  def TEXT, def_version INTEGER,                -- JSON canonical (§6.1)
  enabled INTEGER DEFAULT 0,
  schedule TEXT,                                -- JSON {"every_minutes":N}|{"daily_at":"HH:MM"}|null
  last_scheduled_at TEXT, created_at TEXT, updated_at TEXT);

CREATE TABLE run (
  id TEXT PRIMARY KEY,                          -- uuidv7 == load_id, stamp cột _run_id vào mọi row land
  flow_id TEXT, trigger TEXT,                   -- manual|schedule|mcp|backfill|compaction
  status TEXT CHECK(status IN ('queued','running','success','failed','partial','cancelled')),
  started_at TEXT, ended_at TEXT, error TEXT, updated_at TEXT);
-- PER-FLOW EXCLUSION (finding critical): tối đa 1 run active / flow
CREATE UNIQUE INDEX ux_run_flow_active ON run(flow_id) WHERE status IN ('queued','running');

CREATE TABLE step_run (
  run_id TEXT, step_id TEXT, status TEXT,
  rows_read INTEGER, rows_written INTEGER,
  started_at TEXT, ended_at TEXT, error TEXT, PRIMARY KEY(run_id, step_id));

CREATE TABLE step_interval (                    -- interval accounting (SQLMesh) + resume/backfill
  flow_id TEXT, step_id TEXT, def_version INTEGER,
  interval_start TEXT, interval_end TEXT,
  run_id TEXT, status TEXT,                     -- success|failed
  PRIMARY KEY(flow_id, step_id, interval_start));

CREATE TABLE stream_state (                     -- CHỈ cursor sống; interval nằm ở step_interval
  flow_id TEXT, step_id TEXT,
  cursor_column TEXT, last_value TEXT,
  boundary_hashes TEXT, updated_at TEXT, PRIMARY KEY(flow_id, step_id));

CREATE TABLE lineage_edge (
  run_id TEXT, step_id TEXT, direction TEXT CHECK(direction IN ('in','out')),
  dataset_id INTEGER, schema_version INTEGER);

CREATE TABLE run_log (
  run_id TEXT, seq INTEGER, ts TEXT, level TEXT, step_id TEXT, message TEXT,
  PRIMARY KEY(run_id, seq));

CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT);
-- seed: max_concurrent=2, memory_limit_mb=2048, target_partitions=4,
--       query_max_seconds=600, gc_grace_seconds=1200, log_retention_days=14,
--       import_base64_max_mb=10, import_paths=["<data_dir>/inbox"]
```

Bất biến ghi-state (mở rộng quy tắc "predicate trong UPDATE" của rewrite-story):

- **Watermark monotonic**: `UPDATE stream_state SET last_value=?3 WHERE flow_id=?1 AND
  step_id=?2 AND (last_value IS NULL OR last_value < ?3)` — run chậm không đè watermark mới.
- **step_interval** ghi bằng `INSERT OR REPLACE` từng interval trong transaction commit —
  không bao giờ read-modify-write JSON trong Rust.
- rusqlite: `Mutex<Connection>` **không reentrant** — collect xong drop guard mới gọi
  tiếp; không giữ guard qua `.await`; migrations = idempotent DDL + ALTER nuốt
  "duplicate column" + `schema_version` key trong app_settings cho data-fix một lần.

---

## 5. Connectors — extract & load

### 5.1 Trait + kiểu dữ liệu (định nghĩa đầy đủ — Phase 2 implement thẳng)

```rust
#[async_trait]
trait Connector {
    async fn test(&self) -> Result<()>;
    async fn introspect(&self) -> Result<Vec<TableInfo>>;   // schema/table/column/row-estimate
    async fn extract(&self, spec: ExtractSpec) -> Result<BoxStream<'static, Result<RecordBatch>>>;
    async fn load(&self, spec: LoadSpec, batches: BoxStream<'static, Result<RecordBatch>>) -> Result<u64>;
}

struct ExtractSpec {
    source: SourceRel,                  // Table { schema: Option<String>, name: String } | Query { sql: String }
    columns: Option<Vec<String>>,       // projection; None = *
    cursor: Option<CursorPred>,         // WHERE cursor: { column, op: Ge|Gt, from: Value, to: Option<Value> }
                                        //   Ge = closed-range mặc định (§6.2); to = Some khi backfill chunk
    batch_rows: usize,                  // default 8192
    partition_hint: Option<(String, u32)>, // (cột, số phần) — partition-parallel extract (ý tưởng connectorx), Phase 5
}

struct LoadSpec { target_table: String, mode: LoadMode, create_if_missing: bool }
enum LoadMode {
    FullRefresh,                        // load vào bảng staging `<t>__lake_stage` → swap (RENAME) / TRUNCATE+INSERT nếu DB không swap được
    Append,
    Upsert { keys: Vec<String> },       // PG: INSERT ON CONFLICT; MySQL: ON DUPLICATE KEY
}
```

Mapping Arrow → DDL đích khi `create_if_missing` (chiều ngược của §5.3):
`Utf8→TEXT`, `Int32/Int64→INTEGER/BIGINT`, `Float64→DOUBLE PRECISION`, `Boolean→BOOLEAN`,
`Timestamp(µs,tz)→TIMESTAMPTZ/DATETIME`, `Date32→DATE`, `Decimal128(p,s)→NUMERIC(p,s)`,
`Binary→BYTEA/BLOB`, nested (List/Struct)→`JSONB/JSON/TEXT` (serialize JSON).

### 5.2 Ma trận tier (crate + version xác minh 2026-07-21)

| Tier | Nguồn | Extract | Load | Crates |
|---|---|---|---|---|
| **1 (bundled)** | PostgreSQL (+Redshift/Cockroach) | `sqlx 0.9` stream `fetch()` → ArrayBuilder; fast-path `COPY TO STDOUT (BINARY)` | **`COPY FROM STDIN (FORMAT binary)`**: `tokio-postgres 0.7.18` `BinaryCopyInWriter` + `pgpq 0.11.1` (⚠ pin arrow — §2.3; `finish()` bắt buộc kể cả early-return, không thì COPY abort âm thầm) | sqlx, tokio-postgres, pgpq |
| 1 | MySQL / MariaDB | sqlx stream | `LOAD DATA LOCAL INFILE` in-memory qua `mysql_async 0.37` infile handler; fallback multi-row INSERT (managed MySQL hay tắt local_infile) | sqlx, mysql_async |
| 1 | SQLite | **`rusqlite` sẵn có** (không bật `sqlx-sqlite` — §11) | INSERT batch 1 transaction | rusqlite |
| 1 | File (CSV/JSON/NDJSON/Excel/Parquet) | sniffer copy từ `apps/ontology/src/ingest.rs` + Parquet native | `COPY TO` DataFusion / ghi file exports | datafusion, calamine |
| **1.5 (feature-gate)** | ClickHouse | `clickhouse 0.15.1` RowBinary | HTTP `INSERT … FORMAT ArrowStream` (reqwest thuần — ClickHouse ăn Arrow IPC native) | clickhouse |
| 1.5 | MongoDB | `mongodb 3.8` cursor → `serde_arrow` (infer schema N doc đầu) | `insert_many` batch | mongodb, serde_arrow |
| 1.5 | SQL Server | `tiberius 0.12.3` (⚠ stale từ 7/2024 — driver TDS native duy nhất; sẵn sàng vendor/fork) | `Client::bulk_insert` | tiberius |
| 1.5 | Snowflake | `snowflake-api 0.14` — Arrow ^57 → **IPC tại biên** (§2.3) | `PUT` stage + `COPY INTO` | snowflake-api |
| 1.5 | BigQuery | Storage Read (Arrow ^53 → IPC tại biên) | GCS Parquet + load job | google-cloud-bigquery 0.15 / gcp-bigquery-client 0.28 |
| 1.5 | Oracle | `oracle 0.6.3` (blocking, ODPI-C, cần Instant Client runtime) → spawn_blocking | array-DML `Batch` | oracle |
| **2 (feature `odbc`)** | Teradata/DB2/HANA/Databricks/… | `odbc-api 29` + `arrow-odbc 25.3` columnar bulk fetch | arrow-odbc insert (beta) | cần unixODBC + vendor driver (macOS ARM có pain linking) |

**Không dùng `connectorx`** làm dependency: extract-only, pin arrow ^54 lệch ecosystem,
Python-first maintenance — nhưng **copy ý tưởng** partition-parallel extract. sqlx:
`runtime-tokio` + `tls-rustls-aws-lc-rs` (core HTTP stack của repo chuẩn rustls —
reqwest/tungstenite/teloxide đều bật rustls; nhất quán theo); SQL build động bọc
`AssertSqlSafe` + quote identifier qua allowlist introspect.

### 5.3 Extract → Arrow

Row stream → `ArrayBuilder` theo cột, flush `RecordBatch` mỗi `batch_rows` (map:
int/float/text/bool/timestamp/date/decimal→(i128,scale)/bytes; json/không-map-được →
utf8 + ghi note vào schema change). Batch → `AsyncArrowWriter` (zstd mặc định) thẳng
vào file đích `lake/…/part-<run_id>-<seq>.parquet` (vô hình tới khi commit manifest).

---

## 6. Flow engine — ETL/ELT

### 6.1 DSL khai báo

JSON canonical trong `flow.def`; API/UI/MCP nhận cả YAML string (sniff: bắt đầu `{` →
JSON, ngược lại YAML; parse bằng fork còn maintain — `serde_yaml` gốc đã archive, chốt
`serde_yaml_ng`/saphyr lúc implement; luôn normalize về JSON trước khi lưu).

```yaml
version: 1
flow: shop_analytics
sources:
  - id: orders_raw
    connection: pg_main
    table: public.orders                 # HOẶC query: "SELECT …" (một trong hai, bắt buộc)
    mode: incremental_merge
    cursor: { column: updated_at, initial: "2024-01-01", lag: "1h" }
    primary_key: [order_id]
    merge_key: [order_date]              # bắt buộc ⊆ target.partition_by (§6.2)
    target: { namespace: raw, dataset: orders_raw, partition_by: [order_date] }
    schema_policy: { new_columns: evolve, type_change: variant }
  - id: customers_hist
    connection: pg_main
    table: public.customers
    mode: snapshot
    snapshot: { strategy: timestamp, updated_at: modified_at, unique_key: [customer_id], hard_deletes: new_record }
    target: { namespace: raw, dataset: customers_hist }   # SCD2 tự partition theo _is_current (§6.2)
transforms:
  - id: daily_revenue
    kind: incremental_by_time            # full | incremental_by_time
    time_column: order_date
    interval: day
    lookback: 2
    target: { namespace: marts, dataset: daily_revenue }  # partition_by tự suy = bucket(time_column, interval)
    sql: |
      SELECT order_date, sku, SUM(amount) AS revenue
      FROM orders_raw
      WHERE order_date BETWEEN @start AND @end
      GROUP BY 1, 2
exports:
  - id: revenue_to_pg
    input: daily_revenue
    connection: pg_warehouse             # HOẶC format: csv|parquet|json → exports/
    table: analytics.daily_revenue
    mode: full_refresh                   # LoadMode §5.1
```

**Schema field-by-field** (validate ở `lake_flow_create`/`PUT /flows/:id`; lỗi trả danh
sách `{step_id, field, message}`):

| Field | Kiểu | Bắt buộc | Default / miền giá trị |
|---|---|---|---|
| `flow` | string | ✔ | id: `[a-z0-9_-]{1,64}` |
| sources[].`id` | string | ✔ | unique trong flow |
| sources[].`connection` | string | ✔ | FK bảng connection |
| sources[].`table` / `query` | string | 1 trong 2 | — |
| sources[].`mode` | enum | ✔ | `full_refresh` \| `incremental_append` \| `incremental_merge` \| `snapshot` |
| sources[].`cursor` | object | khi incremental_* | `{column ✔, initial ✔, lag?}` — lag: duration `"1h"/"2d"` |
| sources[].`primary_key` | string[] | khi merge/snapshot | — |
| sources[].`merge_key` | string[] | khi merge | **phải ⊆ `target.partition_by`** |
| sources[].`target` | object | — | default `{namespace:"raw", dataset:<step_id>}`; `partition_by?` |
| sources[].`schema_policy` | object | — | `{new_columns: evolve\|freeze\|discard, type_change: variant\|freeze\|discard}`; default evolve/variant |
| transforms[].`kind` | enum | ✔ | `full` \| `incremental_by_time` |
| transforms[].`time_column`,`interval`,`lookback` | — | khi incremental_by_time | interval: `hour`\|`day`\|`week`\|`month`; lookback: int ≥0 (đơn vị = interval) |
| transforms[].`target` | object | — | default `{namespace:"marts", dataset:<step_id>}` |
| transforms[].`sql` | string | ✔ | SELECT-only, macro `@start`/`@end` khi incremental_by_time |
| exports[].`input` | string | ✔ | id của step trong flow |
| exports[].`connection`+`table` / `format` | — | 1 trong 2 | format → file trong `exports/` |
| exports[].`mode` | enum | ✔ | LoadMode: `full_refresh` \| `append` \| `upsert` (kèm `keys`) |

**Phân giải tên trong SQL transform**: mỗi step id của flow được đăng ký làm **alias
bảng** trong SessionContext của flow (`orders_raw` ↔ dataset `raw.orders_raw`); dataset
ngoài flow phải tham chiếu đầy đủ `<namespace>.<dataset>`. DAG suy từ các alias/tên
xuất hiện trong `FROM`/`JOIN` + `exports[].input`; **một dataset chỉ được đúng 1 flow
ghi** (`dataset.owner_flow_id`) — validate từ chối flow thứ hai trỏ vào cùng target.

### 6.2 Sync modes — ngữ nghĩa + ràng buộc vật lý Parquet

```rust
enum SyncMode {
    FullRefresh,                                  // file mới active + toàn bộ file cũ tombstone — 1 txn manifest
    IncrementalAppend,                            // thêm file active
    IncrementalMerge { strategy: MergeStrategy }, // DeleteInsert (mặc định) | Upsert | InsertOnly
    Snapshot { strategy: SnapshotStrategy },      // Timestamp{updated_at} | Check{cols|all}
}
enum TransformKind { Full, IncrementalByTime }    // Full = rebuild toàn bộ (idempotent, cho bảng dimension)
```

- **Cursor bộ tứ dlt**: `initial_value` / `start_value` / `last_value` / `end_value`
  (set → backfill chunk stateless, không đụng watermark sống). Closed-range `>=` mặc
  định + dedupe boundary bằng `boundary_hashes`; `lag` đọc lùi cửa sổ late-data.
- **2 vai trò key**: `primary_key` = identity dedupe/upsert; `merge_key` = phạm vi
  partition quyết định row vắng mặt nào là delete. Merge thiếu key → **hard-error**
  (không silent-fallback về append — giấu typo config).
- **Ràng buộc vật lý của merge trên Parquet bất biến** (finding major — nói thẳng):
  - `incremental_merge`/`snapshot` **bắt buộc target có `partition_by`**, và
    `merge_key ⊆ partition_by`. Dataset không partition → validate từ chối, trừ khi
    khai `allow_full_rewrite: true` (chấp nhận rewrite TOÀN BỘ dataset mỗi run — nói rõ
    cost cliff trong message).
  - `Upsert` theo PK: xác định partition chứa bản cũ của PK bằng **stats min/max per-file
    trong manifest** (`dataset_file.stats`) — prune trước, chỉ scan file nghi ngờ; PK
    nằm ngoài mọi range → insert mới. Row **đổi giá trị cột partition** giữa 2 version:
    phát hiện trong bước locate → rewrite **cả** partition cũ lẫn mới (không thì PK trùng
    vĩnh viễn — bug câm).
  - `DeleteInsert`: tombstone file của các partition khớp merge_key + ghi file mới — 1 txn.
  - **SCD2 layout**: partition theo `_is_current` — mỗi run rewrite partition
    `_is_current=true` (nhỏ) + append row đóng vào `_is_current=false`; cột meta
    `_valid_from`,`_valid_to`,`_row_hash`,`_is_deleted`; unique theo cặp
    `(row_hash, valid_from)`; hard-delete chỉ suy được từ **full extract**.
- **incremental_by_time transform**: target **luôn tự partition theo bucket của
  `time_column` ở granularity `interval`** (engine tự suy — user không cần khai);
  "delete interval" = tombstone file các partition trong `[start,end)` + active file mới,
  1 txn. `Full` transform = như FullRefresh.
- **Idempotency**: `Full` + `incremental_by_time` = idempotent, restatement từng phần OK;
  merge-by-key và SCD2 = **không idempotent**.
- **Backfill — một quy tắc duy nhất** (dùng nguyên văn ở §6.2, §9 tool, skill operator):
  backfill là **per-step**; step idempotent nhận range interval (chunk stateless qua
  `end_value`, tiến độ ghi `step_interval`, không đụng watermark); step merge/SCD2 bị
  **SKIP mặc định** (transform backfill đọc trạng thái hiện tại của upstream); muốn làm
  lại merge/SCD2 phải opt-in `rebuild: [step_id]` = full-refresh-equivalent (SCD2 rebuild
  **mất lịch sử** — tool bắt confirm).
- **Giới hạn ghi rõ ở mọi nơi**: cursor-based **không phát hiện delete ở nguồn** — chỉ
  full_refresh / snapshot-full / CDC (ngoài scope).

### 6.3 Flow edit vs state (finding major — quy tắc tường minh)

- **State-compatible** (giữ nguyên stream_state/step_interval): đổi SQL text, lookback,
  schedule, batch_rows, schema_policy, description.
- **State-resetting** (reset stream_state + step_interval của step đó, dataset giữ):
  đổi `cursor.column`, `mode`, `primary_key`, `merge_key`, `connection`, `table/query`,
  `time_column`, `interval`. `def_version` tăng mỗi lần đổi.
- Xóa step: dataset giữ (owner ghi nhận), state drop. Đổi `id` step = xóa+thêm (muốn giữ
  state phải dùng `rename: {from,to}` tường minh — mang state theo).
- `lake_flow_update` / `PUT /flows/:id` **trả impact** `{steps_reset:[], steps_kept:[],
  datasets_orphaned:[]}`; thay đổi state-resetting đòi `confirm_reset: true`, thiếu →
  lỗi kèm impact để agent/UI xác nhận.
- Tạo flow trỏ vào **dataset đã có dữ liệu nhưng không có state** (flow cũ bị xóa/đổi
  tên): bắt chọn `adopt` (seed watermark = `MAX(cursor_column)` của dataset) hoặc
  `reset` (từ initial_value — cảnh báo trùng dữ liệu nếu append).

### 6.4 Schema evolution khi land Parquet

Mỗi batch: unify vs catalog → `Unchanged | AddColumns` (nullable — file cũ đọc NULL) `|
Widen` (chỉ lossless: int→long, float→double, decimal mở precision — **giới hạn đúng tập
cast mà SchemaAdapter của DF 54 thực hiện được, có test ghim**) `| Variant` (kiểu đổi
không tương thích → cột `col__v_text`, không fail load). Cột bị xóa ở nguồn: giữ cột,
land NULL. Bump `schema_version` + diff. Không rewrite file lịch sử (trừ compaction).
3 nút chính sách: `evolve` (default) | `freeze` | `discard`.

**Đường đọc** (finding major — load-bearing): `ListingTable` **luôn dựng với
`arrow_schema` hiện tại của catalog** — cấm schema inference (mặc định DF lấy schema
file đầu tiên theo thứ tự list không xác định → cột mới tàng hình hoặc lỗi đọc file cũ).
Parquet match cột theo **tên** — `column_id` trong catalog chỉ phục vụ bookkeeping diff,
KHÔNG cho rename-safety; đổi tên cột = AddColumns + cột cũ NULL. Test mixed-schema
(AddColumns + từng case Widen giữa các file) là deliverable Phase 1.

### 6.5 Runner — pattern rewrite-story + per-flow exclusion

- **Queue DB-backed**: enqueue = `run(status='queued')` — **unique partial index
  `ux_run_flow_active` chặn 2 run active cùng flow** (scheduler tick gặp conflict →
  skip lặng; manual/MCP → 409/isError "flow đang chạy"); claim nguyên tử
  `UPDATE … SET status='running' WHERE id=? AND status='queued'`.
- **Slot + cancel**: `JobGuard` RAII (release cả khi panic), `CancelToken =
  Arc<AtomicBool>` poll giữa batch/step; `max_concurrent` đọc settings mỗi tick.
- **Boot reconcile** (theo §2.2): run mồ côi → failed; file không-trong-manifest thuộc
  run không-commit → xóa; sau đó poller mới start.
- **Watchdog**: quét theo `updated_at` (không `created_at`); running kẹt 60' → failed;
  queued bỏ rơi 24h → cancelled.
- **Guarded write**: mọi rule (terminal không hồi sinh, ownership, progress/watermark
  không lùi) là **predicate trong UPDATE** (TOCTOU đã gây sự cố thật ở rewrite-story).
- **Resume/retry**: retry = **run mới** (không hồi sinh run terminal — nhất quán với
  guard); skip-lookup đọc `step_interval` success của cùng `(flow_id, step_id,
  def_version)` — interval đã xong bỏ qua, đúng ngữ nghĩa "chunks left in place".
- **Backpressure**: từ chối enqueue khi tổng queued+running ≥ N (429 / isError).
- **Delete guard**: xóa flow/dataset/connection bị từ chối khi còn run active hoặc còn
  flow tham chiếu.

### 6.6 Scheduling — self-schedule

Không app nào đăng ký core scheduler (grep = 0 hit) → pattern moltbook/shopee:
`tokio::spawn` loop tick 30s, đọc `flow.schedule` + `last_scheduled_at` (persist SQLite,
sống sót restart), đến hạn → enqueue `trigger='schedule'` (unique index tự chặn chồng
run). Flow tạo qua AI mặc định `enabled=false`.

### 6.7 WS events (route `/api/ws/dashboard`)

Envelope `{type, data, timestamp}` (key **`type`** — video-flow từng chết vì đọc
`event`); hub broadcast(256), delta không mang batch data:

| type | payload | UI invalidate? |
|---|---|---|
| `run:status` | `{run_id, flow_id, status}` | ✔ (list runs + flow) |
| `step:progress` | `{run_id, step_id, rows_written, pct}` | ✘ (chỉ update progress bar) |
| `dataset:updated` | `{namespace, name, schema_version, row_count}` | ✔ (datasets) |

---

## 7. Query path (warehouse)

- `engine.rs`: SessionContext per-request; mỗi dataset đăng ký từ **manifest** (danh sách
  file active + `arrow_schema` catalog — §6.4) dưới tên `<namespace>.<dataset>`;
  `SessionConfig::with_information_schema(true)` (embed phải tự bật).
- **Chặn ghi bằng `SQLOptions`** (finding major — filter hand-rolled theo variant
  sqlparser bị lách bởi `EXPLAIN ANALYZE INSERT …`, DF *thực thi* plan con và DF 54 hỗ
  trợ INSERT INTO ListingTable):
  ```rust
  ctx.sql_with_options(sql, SQLOptions::new()
      .with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false))
  ```
  — `verify_plan` kiểm cả cây LogicalPlan, kể cả DML lồng trong Explain. Parse-first
  (DFParser::parse_sql) chỉ để chặn multi-statement + báo lỗi thân thiện.
- **Reader isolation**: query snapshot danh sách file lúc plan; GC chỉ xóa tombstone sau
  `gc_grace_seconds` (mặc định 1200 = 2× `query_max_seconds`) → không NotFound giữa
  chừng, không đọc 2 thế hệ trộn nhau. Query timeout `query_max_seconds` enforce ở API.
- Memory: `GreedyMemoryPool` (`memory_limit_mb`, mặc định 2048) + `target_partitions=4`
  (FairSpillPool chia budget theo partition — máy ít RAM spill sớm).
- Kết quả LLM-safe: default 100 row, clamp 1..1000, kèm `total_estimate`, `has_more`,
  `next`; cell text cắt 500 ký tự **trên char boundary** (trap UTF-8 tiếng Việt).

---

## 8. REST surface (`/api/*` — main.rs nest)

Error envelope: **status code + `{"error": string}`** (kiểu rewrite-story): 400 body/DSL
sai (kèm danh sách lỗi validate), 404 id lạ, 409 delete-guard/flow-đang-chạy/thiếu
`confirm_reset`, 429 queue đầy, 500 nội bộ. Message khớp text isError của MCP (handler
dùng chung). `GET /api/status` = health (manifest healthPath) trả
`{ok, version, datasets, runs_active}`.

| Route | Verb | Ghi chú |
|---|---|---|
| `/status`, `/health` | GET | health |
| `/ws/dashboard` | GET | WS §6.7 |
| `/datasets` | GET | `?namespace=&limit=&offset=` |
| `/datasets/:ns/:name` | GET | schema + versions + files summary + owner flow |
| `/datasets/:ns/:name/preview` | GET | `?limit=` clamp 200 |
| `/datasets/:ns/:name/lineage` | GET | up/downstream |
| `/datasets/:ns/:name` | DELETE | 409 khi còn run active / flow owner |
| `/datasets/:ns/:name/compact` | POST | run `trigger='compaction'` |
| `/import` | POST | JSON `{filename, contentBase64, namespace?, dataset?}` (model ontology api.rs:253) — `DefaultBodyLimit` 64MB |
| `/exports/:file` | GET | download file exports/ |
| `/connections` | GET/POST | POST `{id?, kind, dsn}`; list luôn redact DSN |
| `/connections/:id/test` | POST | |
| `/connections/:id/introspect` | GET | schema/table/column |
| `/connections/:id` | DELETE | 409 khi flow tham chiếu |
| `/flows` | GET/POST | POST `{def, enable?}` — def = object JSON hoặc string YAML |
| `/flows/:id` | GET/PUT/DELETE | PUT trả impact §6.3, đòi `confirm_reset` |
| `/flows/:id/run` | POST | 409 nếu đang active; trả `{run_id}` |
| `/flows/:id/backfill` | POST | `{start, end, steps?, rebuild?}` — quy tắc §6.2 |
| `/flows/:id/enable` | POST | `{enabled: bool}` |
| `/query` | POST | `{sql, limit?, offset?}` — SQLOptions §7 |
| `/query/explain` | POST | `{sql}` |
| `/query/export` | POST | `{sql, format}` → file exports/ + đường dẫn |
| `/runs` | GET | `?flow_id=&status=&limit=&offset=` |
| `/runs/:id` | GET | per-step + intervals |
| `/runs/:id/cancel` | POST | |
| `/runs/:id/logs` | GET | `?tail=` clamp 500 dòng (bảng `run_log`; sweep theo `log_retention_days` trong maintenance tick) |
| `/settings` | GET/PUT | app_settings |
| `/mcp/sse`, `/mcp/message` | GET+POST / POST | §9 |

REST ↔ MCP parity: handler nghiệp vụ dùng chung; tên tham số MCP = tên tham số REST 1:1.

---

## 9. MCP — `lakehouse-mcp` (~25 tools, prefix `lake_`)

Transport chuẩn: `/api/mcp/sse` (GET SSE + POST) + `/api/mcp/message` (POST — peer app
gọi thẳng). `serverInfo.name == "lakehouse-mcp"`. **Không mirror kết quả tools/call lên
SSE broadcast** (bài học rewrite-story). Test catalogue↔dispatch (`every_advertised_tool_is_dispatchable`)
có **từ Phase 1**. Mọi kết quả: `{"content":[{"type":"text","text":<pretty JSON>}]}`
(+`isError:true`), kèm field `next` gợi ý bước tiếp.

Nhóm & tool (tham số = REST 1:1 trừ khi ghi rõ; inputSchema đầy đủ cho tool nặng contract):

| Nhóm | Tool | inputSchema chốt |
|---|---|---|
| Connection | `lake_connection_add` | `{id?: string, kind: enum, dsn: string}` — test trước khi lưu |
| | `lake_connection_list` / `lake_connection_test` / `lake_connection_delete` | DSN **luôn redact** (`postgres://user:•••@host/db`) |
| | `lake_db_introspect` | `{connection_id, schema?}` |
| Dataset | `lake_dataset_list` / `lake_dataset_schema` / `lake_dataset_delete` | |
| | `lake_dataset_preview` | `{namespace, dataset, limit?: 1..200 default 50}` |
| | `lake_import_file` | `{filename: string, content_base64?: string, path?: string, namespace?, dataset?}` — base64 cap `import_base64_max_mb` (10MB, lỗi trỏ sang path); **path chỉ trong allowlist** `import_paths` (mặc định `inbox/`) — chặn local-file-disclosure qua MCP; Excel = base64/path (calamine) |
| | `lake_dataset_export` | `{namespace, dataset, format: csv\|json\|parquet}` — ghi file đầy đủ vào exports/, trả path + cửa sổ inline (pattern rs_story_export) |
| | `lake_dataset_compact` | gộp file nhỏ theo partition |
| Query | `lake_query` | `{sql: string, limit?: 1..1000 default 100, offset?: default 0}` — SELECT-only (SQLOptions); mô tả: "LUÔN dùng limit — dataset có thể hàng triệu row" |
| | `lake_query_explain` | `{sql}` |
| Flow | `lake_flow_create` | `{def: object\|string(YAML), enable?: bool default false}` — trả DAG đã suy để agent kiểm |
| | `lake_flow_update` | `{flow_id, def, confirm_reset?: bool}` — trả impact §6.3 |
| | `lake_flow_list` / `lake_flow_get` / `lake_flow_delete` | |
| | `lake_flow_generate` | `{description, connection_id?}` — bridge `llm.request` + introspect; trả **draft**, không auto-enable |
| Run | `lake_flow_run` | `{flow_id}` — async, trả run_id + "poll bằng lake_run_status, ĐỪNG chờ đồng bộ"; 409-isError nếu flow đang chạy |
| | `lake_flow_backfill` | `{flow_id, start, end, steps?: string[], rebuild?: string[]}` — quy tắc duy nhất §6.2 (idempotent nhận range; merge/SCD2 skip mặc định, rebuild phải confirm) |
| | `lake_run_status` / `lake_run_list` / `lake_run_cancel` / `lake_run_logs` | logs: `{run_id, tail?: default 100 clamp 500}` |
| Meta | `lake_lineage` | `{namespace, dataset, depth?: default 2}` |
| | `lake_stats` | tổng quan: datasets/size/runs 24h/flow đến hạn |

Bridge: `llm.request` cho `lake_flow_generate` + NL→SQL (nhớ: **không có temperature**,
`finish=="length"` = lỗi, maxTokens ≤ 32000, hand-roll POST vì SDK không cho `profile`).

---

## 10. Skills & persona

- **`lakehouse-analyst`**: quy trình trả lời câu hỏi dữ liệu — `lake_dataset_list` →
  `lake_dataset_schema` trước, `lake_query` với limit, `lake_query_explain` trước query
  nặng; trích số kèm tên dataset + thời điểm run gần nhất; tool id đầy đủ dạng
  `mcp__lakehouse-mcp__lake_query`.
- **`lakehouse-operator`**: vận hành ETL — tạo connection (nhắc user tự cung DSN, không
  đoán password), introspect, `lake_flow_generate` → review DAG → `lake_flow_create` →
  `lake_flow_run` → poll → bật schedule; xử lý sự cố (`lake_run_logs`, phân biệt lỗi
  transient vs schema drift, khi nào backfill vs full_refresh vs rebuild); nêu giới hạn
  (không detect delete nguồn; merge/SCD2 không idempotent — backfill skip mặc định,
  rebuild mất lịch sử SCD2; sửa cursor/PK là state-resetting cần confirm).
- **Persona `data-engineer`**: tổng hợp 2 skill, ưu tiên an toàn dữ liệu, tiếng Việt.

---

## 11. Ràng buộc workspace & rủi ro (đã xác minh, đã sửa theo review)

| Rủi ro | Chi tiết | Đối sách |
|---|---|---|
| sqlx-sqlite | `sqlx-sqlite 0.9` cần libsqlite3-sys `>=0.30.1,<0.38` — **có** unify được với rusqlite 0.32 (^0.30.1), không phải "cargo refuse"; nhưng vẫn không dùng | sqlx chỉ bật `postgres,mysql` (không kéo libsqlite3-sys); SQLite qua rusqlite sẵn có — 1 driver sqlite duy nhất, đỡ binary size |
| Binary size vs zip | Giới hạn 50MB áp lên **zip đã nén** (space.rs:939), route còn `DefaultBodyLimit 64MB` (core.rs:632-635); số 68–100MB của datafusion#13816 là datafusion-cli **full-feature chưa strip** — bản strip + default-features=false nén deflate nhiều khả năng lọt | **Đo ngay Phase 1** (load-bearing, không phải thủ tục); nếu vượt: nâng **CẢ HAI** hằng số (space.rs:939 + core.rs:635 — 2 dòng 2 file, PR riêng); zstd-zip không khả thi với pack.sh hiện tại (Info-ZIP chỉ deflate) — muốn thì thay packer bằng Rust zip writer; dev dùng `register-local` không đụng limit |
| Compile time | dep tree DF nhiều phút cold; `[profile.dev.package."*"] opt-level=3` + release `lto=thin, codegen-units=1`; CI cache chung | chấp nhận (per-app build); cân nhắc chưa thêm RUST_APPS |
| Arrow lockstep | DF 54 pin ^58.3; arrow 60 breaking 8/2026; pgpq `>=56` không chặn trên → resolve 59 nếu không pin; snowflake ^57; bigquery ^53 | quy tắc biên §2.3 + CI check 1 version arrow-array; nâng DF/arrow/parquet/object_store/pgpq như MỘT bộ |
| Feature unification | feature trên dep chung lan cả workspace (--workspace/rust-analyzer) | không bật gì trên dep chung; mirror version app khác (tokio 1, axum 0.7, tower-http 0.5, reqwest 0.12 rustls) |
| SQL injection / ghi lậu / path escape | `EXPLAIN ANALYZE INSERT` lách variant-filter; mapping từng là lỗ SPARQL-injection ở ontology | `SQLOptions` verify cả plan (§7); identifier allowlist + `AssertSqlSafe` phía connector; `object_store` root = `lake/`; import path allowlist `inbox/` (§9); DSN không bao giờ trả nguyên văn |
| DSN secrets | dsn plaintext trong catalog.sqlite (app local single-user — như CRM lưu token) | redact mọi đầu ra; catalog 0600; ghi rõ trong README |
| TLS | core HTTP stack chuẩn rustls (reqwest/tungstenite/teloxide); workspace **không** openssl-free (git2 của core, native-tls của apps/email) | sqlx dùng `tls-rustls-aws-lc-rs` cho nhất quán; không viện dẫn "repo rustls-only" |
| tiberius stale | last release 7/2024 | feature-gate `mssql`, đánh dấu beta, sẵn sàng vendor/fork |
| MemoryPool | spill sớm khi target_partitions cao, RAM ít | Greedy 2GB + target_partitions=4, expose settings |
| UTF-8 tiếng Việt | `&s[..N]` panic multibyte | luôn truncate trên char boundary |

---

## 12. Kế hoạch triển khai + test theo phase

| Phase | Nội dung | Test bắt buộc (co-located `#[cfg(test)]` — chuẩn repo) |
|---|---|---|
| **1 — Lake core** | Scaffold (manifest đầy đủ description/triggers, README, pack.sh, workspace member); catalog 10 bảng; `lake.rs` manifest-commit + GC + reconcile; `lake_import_file` (sniffer ontology); `engine.rs` manifest→ListingTable + SQLOptions; MCP dataset_*+query+stats; UI Datasets+Query. **Đo binary/zip size ngay.** | catalogue↔dispatch sync; mixed-schema read (AddColumns + từng case Widen); manifest-commit atomicity (file vô hình trước commit); reconcile xóa file run không-commit; SQLOptions chặn `EXPLAIN ANALYZE INSERT`; clamp + char-boundary |
| **2 — Connections + Extract** | Trait Connector + ExtractSpec/LoadMode; postgres/mysql (sqlx) + sqlite (rusqlite); introspect; `full_refresh` + `incremental_append`; DSL sources-only + validate; runner (claim/cancel/reconcile/watchdog + unique active/flow); MCP connection_*+flow_*+run_*; UI Connections/Flows/Runs + WS | cursor bộ tứ + closed-range + boundary dedupe; watermark monotonic guard; claim + unique-active-per-flow (2 enqueue cùng flow); watchdog updated_at; DSL validate error list |
| **3 — Transform + Flow đầy đủ** | `full` + `incremental_by_time` (@start/@end, lookback, step_interval); `incremental_merge` (stats-prune, partition-value-change) + `snapshot` SCD2 (_is_current layout); schema evolution + policy; DAG từ SQL; scheduling; backfill (quy tắc §6.2); flow-edit impact (§6.3); lineage; `lake_flow_generate` | interval accounting + resume-skip; merge: PK đổi partition → rewrite 2 partition; SCD2 (row_hash, valid_from) unique + reinstate; backfill skip merge/SCD2 mặc định + rebuild confirm; flow-edit impact matrix; DAG derivation |
| **4 — Load/Export + vận hành** | Export connector (PG BinaryCopy+pgpq theo §2.3, MySQL INFILE, file); `lake_dataset_export`; compaction; skills+persona; UI DAG view + NL→SQL; e2e | LoadMode 3 nhánh; pgpq finish()-on-error; compaction giữ query ổn (GC grace); e2e flow chạy → query khớp |
| **5 — Connector mở rộng** | Feature-gate: clickhouse, mongodb, mssql, snowflake (IPC biên), bigquery (IPC biên), oracle, odbc; partition-parallel extract | per-connector roundtrip nhỏ + IPC-boundary conversion |

Phase 1–2 là lõi giá trị (import + query + sync); mỗi phase ship độc lập.

---

## 13. Nguồn tham khảo chính

- DataFusion 54 / arrow 58–59 / object_store 0.13–0.14 / deltalake 0.32.4 / iceberg 0.9.1:
  crates.io + datafusion.apache.org (ddl/dml/information_schema/UDF/memory_pool/
  **SQLOptions**) + apache/datafusion#13816 + apache/iceberg-rust#2186.
- duckdb-rs 1.10504.0 / DuckDB 1.5.4 / DuckLake 1.0: duckdb.org, duckdb-rs #378/#461/PR#732,
  blog.colinbreck.com (offline extensions), datafusion-contrib/datafusion-ducklake.
- Connectors: sqlx 0.9, tokio-postgres 0.7.18, pgpq 0.11.1 (deps arrow `>=56` — crates.io),
  mysql_async 0.37, tiberius 0.12.3, oracle 0.6.3, mongodb 3.8 + serde_arrow,
  odbc-api 29 + arrow-odbc 25.3, snowflake-api 0.14 (deps arrow ^57), google-cloud-bigquery
  0.15 (arrow ^53), clickhouse 0.15.1 + ArrowStream, adbc_core 0.23, connectorx 0.4.5 (prior art).
- ETL semantics: dlt (cursor/merge/schema-contracts/SCD2 + **filesystem-destination từ
  chối merge trên plain parquet** — lý do chuyển manifest), Airbyte (sync modes, checkpoint),
  SQLMesh (model kinds, interval restatement), dbt (snapshots, artifacts), OpenLineage,
  Redpanda Connect (DSL shape), Iceberg spec, Delta type-widening.
- Nội bộ (file:line đã verify): apps/search + apps/json (template), apps/ontology
  (ingest sniffer, api.rs:253 upload), apps/rewrite-story (runner/watchdog/guarded
  updates, api.rs route style, mcp.rs:617 catalogue test), src/gateway/ui_server/space.rs:939
  (50MB) + core.rs:632-635 (DefaultBodyLimit 64MB) + space_mcp.rs (launch/env),
  root Cargo.toml (pins, profile).
