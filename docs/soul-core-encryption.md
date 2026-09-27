# Mã hoá Soul Core bằng mật khẩu — nghiên cứu thiết kế

> **Trạng thái: HOÃN** · chốt 2026-09-12 · xét lại **2027-03**.
> Lý do: hồ sơ đã ở quyền 0600 và `SENCLAW_AUTH_MODE=strict` đã chặn app khác
> đọc qua API. Một vault có mật khẩu chỉ đáng giá khi có đồng bộ đa máy —
> chưa có. Đây là thiết kế, **không phải tính năng đang chạy**.


Nghiên cứu cơ chế **mã hoá dữ liệu core của SenClaw bằng một mật khẩu do người
dùng đặt**: hỏi ở lần chạy đầu, dùng làm khoá cho hồ sơ người dùng
([`USER.md`](soul-core-user-profile.md)) và các dữ liệu nhạy cảm khác, kèm bộ MCP
tool để agent cất/lấy bí mật.

Trạng thái: **nghiên cứu, chưa code**. Tài liệu này chốt mô hình đe doạ, so sánh
lựa chọn thuật toán, chỉ ra **một mâu thuẫn trong đặc tả ban đầu** và cách giải,
rồi đề xuất thiết kế + kế hoạch.

---

## 1. Mô hình đe doạ — phải chốt trước, vì nó quyết định mọi thứ còn lại

Mã hoá at-rest bằng mật khẩu bảo vệ **một tập rất cụ thể** các tình huống. Nói rõ
ngay từ đầu để không xây nhầm kỳ vọng:

| Tình huống | Có bảo vệ? |
|---|---|
| Mất/trộm laptop, ổ cứng bị tháo ra đọc | **Có** |
| Backup (Time Machine, iCloud, rsync, Dropbox) lọt ra ngoài | **Có** |
| Tài khoản OS khác trên cùng máy đọc `~/.senclaw/` | **Có** (cộng với quyền 0600) |
| Vô tình commit / gửi file `senclaw.db` cho người khác | **Có** |
| Malware chạy **dưới chính user đó, khi vault đang mở** | **Không** |
| Người dùng lỡ dán bí mật vào khung chat | **Không** |
| Agent bị prompt-injection rồi tự gọi tool để lộ bí mật | **Không** — phải chặn bằng cơ chế khác (§6.4) |

Điều này khớp với ghi chú đã có trong [CLAUDE.md](../CLAUDE.md) về token Space App:
*"Strict mode is not a boundary against local malware. Anything that can read
`~/.senclaw/senclaw.db` reads every token in it."* Mã hoá bằng mật khẩu **nâng
được đúng câu đó** — file `.db` bị đọc trộm không còn tự động lộ token — nhưng
chỉ khi vault đang khoá.

**Hệ quả thiết kế**: giá trị lớn nhất nằm ở trạng thái *khoá*. Nếu daemon tự mở
vault khi khởi động (lưu mật khẩu ở đâu đó để tự nhập) thì toàn bộ tính năng chỉ
còn là mã hoá trang trí. Xem §5.4.

## 2. Hiện trạng: SenClaw đang cất bí mật ở đâu

Đo thực tế trên máy dev, 15/08/2026 (`stat -f "%Sp %z"`):

| Dữ liệu | Nơi lưu | Quyền | Kích thước | Bảo vệ |
|---|---|---|---|---|
| API key LLM | `~/.senclaw/config.json` ([`types.rs:239`](../src/gateway/group_manager/types.rs)) | **`-rw-r--r--`** | 10 KB | **Không** |
| Token OAuth provider | `~/.senclaw/oauth.json` | `-rw-------` | 1 KB | 0600 ([`oauth/store.rs:177`](../src/providers/oauth/store.rs)) |
| Token API daemon | `~/.senclaw/api_token` | `-rw-------` | 64 B | 0600 |
| Bot token kênh | `channels.credentials_json` | **`-rw-r--r--`** | — | **Không** |
| Token Space App (`sca_*`) | `space_app_tokens` | **`-rw-r--r--`** | — | **Không** |
| Lịch sử hội thoại | `channel_messages` | **`-rw-r--r--`** | — | **Không** |
| Memory / cognitive graph | `senclaw.db` + `senclaw_cognitive.db` + FTS5 | **`-rw-r--r--`** | 51 MB + 26 MB | **Không** |
| Cấu hình project | `~/.senclaw/project-config.json` | **`-rw-r--r--`** | 2,2 MB | **Không** |
| Hồ sơ người dùng (đề xuất) | `~/.senclaw/USER.md` | — | *chưa tồn tại* | Chưa có |

Ba quan sát từ số đo, không phải suy đoán:

1. **`config.json` là `0644` — world-readable — và đang chứa 4 trường bí mật
   không rỗng** (`apiKey` / `token` / `accessToken` / `encryptionKey`; đếm bằng
   script, không in giá trị). Chỉ `oauth.json` và `api_token` được đặt 0600. Đây
   là một chênh lệch nên vá **ngay, độc lập với vault** — `chmod 600` cho
   `config.json` là một dòng, không cần chờ giai đoạn 5.
2. **`senclaw.db` 51 MB, `senclaw_cognitive.db` 26 MB, `project-config.json`
   2,2 MB — tất cả `0644`.** Bất kỳ tiến trình nào, tài khoản OS nào trên máy đọc
   được. Đây chính là kịch bản §1 mà vault sinh ra để chặn.
3. **`senclaw.db-wal` đang tồn tại và nặng 4,2 MB.** Xác nhận cụ thể điểm §5.4:
   dữ liệu chưa checkpoint nằm trong file WAL riêng; mọi phương án mã hoá phải phủ
   nó, không chỉ file `.db` chính.

Đã có sẵn hạ tầng mã hoá: [`src/util/crypto.rs`](../src/util/crypto.rs) —
AES-256-GCM qua crate `aes-gcm 0.10` (đã trong `Cargo.toml`), hiện chỉ dùng cho
kênh `app`/`senclaw` relay ([`lib.rs:1637`](../src/lib.rs),
[`clawhub/relay_client.rs:93`](../src/clawhub/relay_client.rs)).

**Ba vấn đề của `Crypto` hiện tại nếu tái dùng cho vault:**

1. `new_from_b64` rơi về **SHA-256 một vòng** khi key không đủ 32 byte
   ([`crypto.rs:36`](../src/util/crypto.rs)). Với khoá ngẫu nhiên thì không sao;
   với **mật khẩu người dùng** thì đây là thảm hoạ — SHA-256 một vòng bẻ được
   hàng tỉ lần/giây trên GPU. Vault **bắt buộc** phải dùng KDF chậm (§4.1).
2. `aad: &[]` ([`crypto.rs:60`](../src/util/crypto.rs)) — không ràng buộc ngữ
   cảnh. Kẻ có quyền ghi DB có thể **hoán đổi ciphertext giữa các bản ghi** (đổi
   giá trị ô `smtp_password` sang ciphertext của ô khác) mà AEAD vẫn verify hợp lệ.
   Vault phải đưa `record_id ‖ label ‖ version` vào AAD.
3. `get_key()` trả **bản sao** `[u8; 32]` (`Copy`) — khoá bị nhân bản khắp nơi,
   không có `zeroize`, nằm lại trong stack/heap sau khi dùng.

## 3. Mâu thuẫn trong đặc tả — và cách giải

Đặc tả có hai yêu cầu **không thể đồng thời đúng** như đang viết:

- *"không thể lấy pass qua mcp này"*
- *"thêm mcp tool … 1 mã hoá và lưu **đưa key vào** … 2 lấy thông tin đã mã hoá
  **qua key đưa vào**"*

Nếu tool nhận `key` làm **tham số**, thì chính **LLM** phải sinh ra giá trị đó.
Nghĩa là mật khẩu sẽ:

- nằm trong context cửa sổ của model,
- được **gửi lên nhà cung cấp LLM** (OpenAI/Anthropic/OpenRouter…) trong body request,
- ghi vào lịch sử hội thoại `channel_messages` — **ở dạng thô, trong chính cái DB ta đang muốn mã hoá**,
- lọt vào log tool-call ([`util/llm_log.rs`](../src/util/llm_log.rs)).

Khuyến nghị của ngành năm 2026 thống nhất về điểm này: **không bao giờ truyền bí
mật làm tham số tool** — dùng header / env / dependency injection để tool có được
credential mà LLM không nhìn thấy (xem §9). Một khảo sát được trích dẫn rộng rãi
ghi nhận hơn 12.000 API key và mật khẩu lộ ra vì MCP xử lý credential sai cách.

### Cách giải: tách "mở khoá" khỏi "dùng khoá"

| | Ai làm | Đường đi |
|---|---|---|
| **Mở khoá** (nhập mật khẩu) | **Con người**, qua UI desktop / web / CLI | Không bao giờ qua MCP, không bao giờ qua LLM |
| **Dùng khoá** (cất/lấy bí mật) | **Agent**, qua MCP tool | Tool **không có tham số key**; daemon dùng DEK đang giữ trong RAM |

Tool vẫn có đúng hai hàm chính như yêu cầu — chỉ là chữ ký bỏ tham số `key`:

```
vault_store(label, value, category?)   → lưu đã mã hoá
vault_get(label, reveal?)              → lấy ra
```

Khi vault đang khoá, tool trả về `{"status": "locked"}` và **hướng dẫn agent bảo
người dùng mở khoá** — không bao giờ trả plaintext, không bao giờ nhận mật khẩu.

Đây chính là cách SenClaw đã giải một bài toán cùng dạng: `space_app_*` không tự
quản process mà **gọi loopback ngược vào daemon**, vì state thật nằm trong bộ nhớ
của daemon ở process khác (xem [CLAUDE.md](../CLAUDE.md) §"Managing Space Apps
from chat"). MCP server chạy trong process `senclaw core-server` **tách khỏi
daemon**, nên nó *không thể* chạm DEK trực tiếp — và đó là một tính chất tốt, nên
giữ.

> Nếu vẫn muốn đúng nghĩa đen "đưa key vào tool", xem §6.5 — có một chế độ phụ,
> yếu hơn, dùng passphrase riêng cho từng mục và chấp nhận nó nằm trong transcript.

## 4. Nghiên cứu thuật toán

### 4.1 Dẫn xuất khoá từ mật khẩu (KDF)

| Thuật toán | Đánh giá |
|---|---|
| SHA-256 một vòng | **Không dùng.** Là cách `crypto.rs` đang làm; GPU bẻ hàng tỉ lần/giây |
| PBKDF2-SHA256 | Chấp nhận được, nhưng kháng GPU/ASIC kém vì không tốn RAM. SQLCipher mặc định dùng cái này |
| scrypt | Tốt, có tham số bộ nhớ |
| **Argon2id** | **Khuyến nghị.** Người thắng Password Hashing Competition; lai Argon2i+Argon2d nên kháng cả side-channel lẫn GPU |

Tham số: OWASP nêu mức **tối thiểu** `m=19456 KiB (19 MiB), t=2, p=1`. Đó là mức
cho web server phải hash hàng nghìn lần/giây — SenClaw chỉ dẫn xuất **một lần khi
mở khoá**, nên chi trả được cao hơn nhiều.

Đề xuất mặc định: **`m = 64 MiB, t = 3, p = 1`** (~0,3–0,8 s trên máy để bàn hiện
đại — đủ chậm cho kẻ tấn công, đủ nhanh cho người dùng).

**Bắt buộc: ghi tham số KDF vào file keyring.** Đổi mặc định ở bản sau mà không
ghi lại tham số cũ = mọi vault cũ không mở được nữa. Đây là lỗi kinh điển.

Lưu ý riêng cho SenClaw: `channel_app` chạy trên **điện thoại** (xem memory
*channel_app migration*). 64 MiB Argon2 trên máy Android đời thấp có thể chậm hoặc
bị OOM. Nếu vault mở rộng sang mobile, hạ xuống `m=32 MiB` cho nền tảng đó và
**ghi tham số vào file** để hai bên vẫn giải mã lẫn nhau được.

### 4.2 Mã hoá đối xứng (AEAD)

| Lựa chọn | Ghi chú |
|---|---|
| **AES-256-GCM** | Đã có sẵn trong repo (`aes-gcm 0.10`). Nhanh khi CPU có AES-NI (mọi máy x86-64/ARM64 hiện đại). Nonce 12 byte → nonce ngẫu nhiên chỉ an toàn tới ~2³² bản ghi/khoá |
| XChaCha20-Poly1305 | Nonce 24 byte → nonce ngẫu nhiên an toàn thực tế vô hạn; nhanh cả khi không có AES-NI. Cần thêm dependency |

**Đề xuất: AES-256-GCM**, tận dụng dependency có sẵn. Với vault, số bản ghi tính
bằng nghìn chứ không phải tỉ, nên giới hạn 2³² không phải rủi ro thực tế.

Hai điều **bắt buộc** khác với `crypto.rs` hiện tại:

- **Nonce ngẫu nhiên mới cho mỗi lần ghi** (đã đúng — `OsRng.fill_bytes`).
- **AAD phải khác rỗng**: `AAD = version ‖ record_id ‖ label`. Không có nó, kẻ có
  quyền ghi DB hoán đổi được ciphertext giữa các bản ghi (§2).

### 4.3 Envelope encryption — mật khẩu **không** trực tiếp mã hoá dữ liệu

```
mật khẩu ──Argon2id(salt, m, t, p)──► KEK (32B, chỉ trong RAM)
                                        │
                                        ├─ AES-256-GCM wrap ──► DEK đã bọc  (lưu ra đĩa)
                                        │
DEK (32B ngẫu nhiên) ──AES-256-GCM──► dữ liệu (USER.md, secrets, …)
```

Lý do bắt buộc phải làm vậy — **đổi mật khẩu chỉ cần bọc lại DEK**, không phải
giải mã rồi mã hoá lại toàn bộ dữ liệu. Không có lớp này thì đổi mật khẩu là một
thao tác migration rủi ro trên toàn DB, và người dùng sẽ không bao giờ đổi.

Phụ phẩm: **không cần lưu "verifier" riêng để kiểm tra mật khẩu đúng/sai.** Thẻ
xác thực AEAD của DEK đã bọc *chính là* verifier — sai mật khẩu thì AEAD decrypt
thất bại. Lưu thêm một chuỗi "known plaintext" đã mã hoá là tự tặng kẻ tấn công
một oracle để dò offline.

### 4.4 Vệ sinh bộ nhớ

- Crate `zeroize` — `Zeroizing<[u8; 32]>` cho KEK và DEK, xoá khi drop.
- Bỏ `get_key()` trả bản sao; đưa khoá vào sau một handle không `Copy`.
- Crate `secrecy` — bọc `SecretString` để không lỡ `{:?}` mật khẩu vào log.
  Đáng lưu ý: repo đã có [`src/safe_log.rs`](../src/safe_log.rs), nên nối vào đó.
- Không thể chống được: swap file, hibernate image, core dump. Ghi rõ trong docs
  thay vì giả vờ có bảo vệ.

### 4.5 Tuỳ chọn "nhớ trên máy này" — OS keychain

Crate `keyring` bọc Keychain (macOS), DPAPI/Credential Manager (Windows),
libsecret (Linux). Cất **DEK** (không phải mật khẩu) vào đó → daemon tự mở khoá
khi khởi động mà không hỏi.

Đánh đổi phải nói thẳng với người dùng: bật cái này thì **mất phần lớn giá trị
của §1** — máy bị trộm khi đã đăng nhập OS thì DEK lấy được. Chỉ nên để dạng
opt-in, mô tả rõ, mặc định **tắt**.

## 5. Nghiên cứu mã hoá dữ liệu trong SQLite

### 5.1 Hai hướng, khác nhau về bản chất

| | **Toàn DB (SQLCipher / sqlite3mc)** | **Từng trường (application-level)** |
|---|---|---|
| Phạm vi | Cả file `.db`, gồm index, WAL, temp | Chỉ ô nào ta chủ động mã hoá |
| Index / FTS5 | Được bảo vệ (nằm trong file) | **Không** — xem §5.2 |
| `SELECT` / `WHERE` trên ô mã hoá | Bình thường | Không dùng được (chỉ so khớp chính xác nếu dùng nonce cố định — mà không được phép) |
| Build | Phải đổi `libsqlite3-sys` | Không đổi gì |
| `sqlite-vec`, extension khác | **Cần kiểm chứng lại toàn bộ** | Không ảnh hưởng |
| Hạt bảo vệ | Tất-cả-hoặc-không | Chọn đúng thứ nhạy cảm |

### 5.2 Cái bẫy lớn nhất: FTS5 giữ plaintext

SenClaw có **hai** index toàn văn:

- `memory_chunks_fts` — [`memory/schema.rs:71`](../src/memory/schema.rs)
- `cog_nodes_fts` — [`memory/cognitive/schema.rs:380`](../src/memory/cognitive/schema.rs)

Nếu chọn hướng "mã hoá từng trường" và mã hoá cột `text` của memory chunk, **bảng
FTS5 vẫn chứa nguyên văn bản thô** — đó là cách nó hoạt động, nó phải tokenize
được. Kết quả: DB nhìn thì thấy "đã mã hoá", nhưng `SELECT text FROM
memory_chunks_fts` trả về đúng nội dung đó. Cùng vấn đề với `cog_nodes_fts`
(name + summary).

Suy ra:

- **Mã hoá từng trường chỉ hợp với dữ liệu KHÔNG được đánh index toàn văn**:
  `channels.credentials_json`, `space_app_tokens`, `llm_configs.apiKey`, bảng
  vault mới. Với đúng nhóm này thì nó rất hợp — chúng vốn chỉ được đọc theo khoá
  chính, không bao giờ `LIKE`/`MATCH`.
- **Muốn bảo vệ memory và cognitive graph thì bắt buộc mã hoá cả file.**

### 5.3 SQLCipher hay SQLite3 Multiple Ciphers?

`rusqlite` hiện dùng `features = ["bundled"]` ([`Cargo.toml:58`](../Cargo.toml)).

- **`bundled-sqlcipher`** — link vào libcrypto của hệ thống (OpenSSL/LibreSSL);
  `bundled-sqlcipher-vendored-openssl` thì build kèm OpenSSL. Cả hai đều **thêm
  gánh nặng đáng kể cho pipeline release đa nền tảng** (macOS + Windows, xem memory
  *Windows build chỉ khi tag*). SQLCipher mặc định dẫn khoá bằng **PBKDF2**, yếu
  hơn Argon2id — nhưng ta sẽ đưa **DEK ngẫu nhiên 32 byte** vào qua
  `PRAGMA key = "x'<hex>'"` (raw key, bỏ qua KDF của nó) nên điểm này thành vô
  hại.
- **SQLite3 Multiple Ciphers (sqlite3mc)** — gộp sẵn nhiều cipher (kể cả bản
  tương thích SQLCipher và ChaCha20-Poly1305), **không phụ thuộc OpenSSL**. Hấp
  dẫn hơn về mặt build. Nhưng tích hợp với `rusqlite` chưa có đường chính thống —
  phải thay `libsqlite3-sys`, **cần dựng thử một prototype trước khi cam kết**.

### 5.4 Những thứ rò rỉ ngoài file `.db`

Mã hoá cả DB vẫn hở nếu quên:

- **WAL và `-shm`** — SQLCipher/sqlite3mc mã hoá WAL, nhưng file `-wal` sót lại
  sau khi crash phải được xử lý cùng khoá.
- **Temp/spill file** — SQLite ghi kết quả sort/join lớn ra đĩa. Đặt
  `PRAGMA temp_store = MEMORY`.
- **`config.json`, `oauth.json`, `api_token`, `USER.md`** — nằm **ngoài** DB. Mã
  hoá DB không chạm tới chúng. Đây là lý do vault phải phủ cả file phẳng, không
  chỉ SQLite.
- **Backup và export** — mọi đường xuất dữ liệu (`/api/...` export, log) phải
  được rà lại.

### 5.5 Đề xuất: làm hai lớp, theo thứ tự

**Lớp 1 (làm trước, rủi ro thấp)** — mã hoá từng trường cho đúng nhóm bí mật:
bảng vault mới, `channels.credentials_json`, `space_app_tokens`, `apiKey` trong
`config.json`, `oauth.json`, và `USER.md`. Không đổi build, không đụng FTS5,
không đụng `sqlite-vec`.

**Lớp 2 (sau, nếu thật cần)** — mã hoá cả file cho `senclaw.db` + `cognitive.db`
để phủ lịch sử hội thoại và memory. Chỉ làm sau khi có prototype chứng minh
`sqlite-vec` và FTS5 vẫn chạy, và đo được chi phí build cho CI Windows/macOS.

Lớp 1 chặn được kịch bản hay xảy ra nhất: file DB hoặc thư mục `~/.senclaw/` lọt
ra ngoài và **kẻ nhặt được dùng luôn token/API key**.

## 6. Thiết kế đề xuất

### 6.1 File keyring

`~/.senclaw/vault.json`, quyền **0600** (theo mẫu [`oauth/store.rs:177`](../src/providers/oauth/store.rs)):

```json
{
  "version": 1,
  "kdf": { "algo": "argon2id", "m_kib": 65536, "t": 3, "p": 1,
           "salt": "<base64 16B>" },
  "dek": { "algo": "aes-256-gcm", "nonce": "<base64 12B>",
           "wrapped": "<base64>", "aad": "senclaw-vault-v1" },
  "created_at": "2026-08-15T…", "unlock_hint": "" 
}
```

Không có verifier riêng (§4.3). `unlock_hint` là gợi ý người dùng tự gõ, mặc định
rỗng — và **không bao giờ** được phép chứa mật khẩu.

### 6.2 Lần chạy đầu

[`src/setup.rs`](../src/setup.rs) đã giải xong phần khó của một câu hỏi lần đầu:
phát hiện TTY (`IsTerminal`), timeout 120 s, **bỏ qua im lặng khi chạy nền/CI**.
Thêm bước hỏi vault vào đó theo cùng khuôn.

**Bẫy phải tránh**: desktop app spawn daemon **không có TTY** (xem memory *Daemon
build & deploy*) → `setup.rs` sẽ bỏ qua im lặng và người dùng desktop **không bao
giờ thấy câu hỏi**. Bắt buộc phải có bản UI song song trong onboarding của
`desktop_app/`, không chỉ CLI.

Quy tắc nhập:

- Tối thiểu 12 ký tự (mật khẩu này không có rate-limit khi bị tấn công offline —
  độ dài là phòng thủ duy nhất). Hiển thị ước lượng độ mạnh.
- Nhập hai lần để xác nhận.
- Cảnh báo **không thể khôi phục** trước khi xác nhận, và bắt tick "tôi đã hiểu".
- Đề nghị tạo **recovery code** (một chuỗi ngẫu nhiên cao entropy bọc DEK lần thứ
  hai) để in/cất ra ngoài. Không có nó thì quên mật khẩu = mất dữ liệu, và đó là
  cách nhanh nhất để người dùng mất niềm tin vào tính năng.
- Chọn "không" là hợp lệ, hoạt động y như hôm nay, và **bật lại được sau** trong
  Settings.

### 6.3 Vòng đời phiên mở khoá

| Trạng thái | Hành vi |
|---|---|
| `disabled` | Không bật vault. Như hiện tại |
| `locked` | Daemon chạy bình thường; secret không đọc được; `USER.md` **không được inject**; tool vault trả `locked` |
| `unlocked` | DEK trong RAM (`Zeroizing`), tự khoá lại sau `N` phút không hoạt động (mặc định 60), và khi shutdown |

**Fail closed ở mọi nhánh.** Không giải mã được → coi như không có dữ liệu, không
bao giờ rơi về plaintext. Điều này khớp với luật đã có trong CLAUDE.md cho ảnh:
*"Never send image blocks on a maybe"* — cùng triết lý.

Daemon phải bắt `SIGTERM` để zeroize (đã bắt rồi, sau lần vá orphan Space App —
xem memory *Daemon orphan Space Apps*).

### 6.4 MCP tool

Theo đúng quy ước đặt tên trong [CLAUDE.md](../CLAUDE.md): server **`senclaw-vault`**,
tiền tố tool **`vault_`**.

| Tool | Tham số | Trả về |
|---|---|---|
| `vault_store` | `label`, `value`, `category?`, `description?` | `{ok, label}` — **không** vọng lại `value` |
| `vault_get` | `label`, `reveal?` (mặc định `false`) | `reveal=false` → metadata + handle `{{vault:label}}`; `reveal=true` → plaintext |
| `vault_list` | `category?` | Danh sách label + metadata, **không có giá trị** |
| `vault_status` | — | `enabled` / `locked` / `unlocked`, thời điểm tự khoá |
| `vault_delete` | `label` | `{ok}` |

**Không có `vault_unlock`.** Đây là điều thoả mãn yêu cầu *"không thể lấy pass qua
mcp"*: không tool nào nhận, trả, hay chạm tới mật khẩu. Mở khoá chỉ từ UI/CLI.

Bốn ràng buộc bắt buộc:

1. **`reveal=false` là mặc định, và nên là đường đi chính.** Trả handle
   `{{vault:smtp_password}}` để **daemon thay thế lúc dùng thật** (khi gửi HTTP
   request, khi ghi config) — bí mật không bao giờ vào context của model. `reveal=true`
   chỉ dùng khi người dùng thực sự hỏi "mật khẩu X là gì".
2. **`reveal=true` phải là hành động cần xác nhận**, đi qua permission bridge
   ([`agent/permission_bridge/`](../src/agent/permission_bridge/)) như một tool
   nguy hiểm. Nếu không, một prompt-injection trong trang web mà agent đọc sẽ đủ
   để rút sạch vault. Lưu ý memory *Agent security / Morris II* đã ghi nhận
   **permission fail-open** từng là lỗi thật trong repo này.
3. **Ghi audit mọi lần truy cập** — label, thời điểm, jid, có reveal hay không.
   Đây là thứ duy nhất phát hiện được vault bị rút trộm sau sự việc.
4. **Không bao giờ inject bí mật vào group chat.** Cùng cổng riêng tư như
   `USER.md` — xem [soul-core-user-profile.md §5.4](soul-core-user-profile.md).

### 6.5 Route loopback — và một cái bẫy nghiêm trọng

MCP server chạy ở process `senclaw core-server`, tách khỏi daemon (xem
[mcp-senclaw-core-bundled.md](mcp-senclaw-core-bundled.md)), nên tool phải gọi loopback ngược
vào daemon — đúng mẫu `space_app_*`.

**Bẫy**: daemon **miễn API token cho mọi peer loopback**
([`ui_server/auth.rs`](../src/gateway/ui_server/auth.rs), xem
[remote-access-security.md](remote-access-security.md)). Nếu route vault kế thừa
điều đó thì **bất kỳ process local nào** — kể cả Space App bất kỳ, kể cả script
người dùng chạy nhầm — cũng `curl` được `/api/vault/reveal`, và toàn bộ thiết kế
sụp đổ.

Route vault **phải tự có xác thực riêng**, không dùng miễn trừ loopback. Mẫu có
sẵn để sao chép: token per-app `SENCLAW_TOKEN_ACCESS_APP`
([`src/apps/token.rs`](../src/apps/token.rs),
[space-app-api-token.md](space-app-api-token.md)) — daemon sinh một capability
token mỗi phiên, đưa vào env của process MCP khi spawn, và chỉ chấp nhận token đó
trên route vault.

Song song: chặn agent đọc thẳng file. Agent có `Bash` và `Read`. Đọc
`~/.senclaw/vault.json` chỉ thu được DEK **đã bọc** (vô dụng nếu không có mật
khẩu) nên không nguy hiểm — nhưng nên đưa `~/.senclaw/vault.json` và `oauth.json`
vào danh sách chặn của `allowed_paths` / per-app sandbox
([space-app-sandbox.md](space-app-sandbox.md)) để không có đường vòng.

### 6.6 Chế độ phụ "khoá rời" — nếu thật sự cần truyền key vào tool

Nếu vẫn cần đúng nghĩa đen chữ ký `encrypt(key, data)` / `decrypt(key)`, đề xuất
tách hẳn thành tool khác tên để không lẫn với vault chính:

```
vault_seal(passphrase, data)    → trả blob mã hoá (agent tự giữ/đưa người dùng)
vault_unseal(passphrase, blob)  → trả plaintext
```

Đặc điểm phải nói rõ trong chính mô tả tool: **passphrase này nằm trong transcript
và được gửi lên nhà cung cấp LLM.** Nó chỉ hợp cho tình huống "mã hoá một đoạn văn
bản để gửi cho người khác qua kênh không an toàn", **không** cho bí mật lâu dài
của hệ thống. Và nó **không** dùng chung DEK với vault chính — hai thứ độc lập,
để một passphrase lộ trong transcript không bao giờ ảnh hưởng tới master password.

## 7. Ảnh hưởng tới `USER.md`

Khi vault bật, `~/.senclaw/USER.md` được lưu dạng `USER.md.enc` (AEAD, AAD gắn
đường dẫn). Đường inject ở
[`collect_first_turn_context`](../src/zen_core/engine.rs) phải giải mã tại chỗ.

Khi vault **khoá**: không inject, và **không báo lỗi cho model** — chỉ đơn giản
không có block `<user_profile>`, y như khi file không tồn tại. Báo cho *người
dùng* qua UI, không qua prompt: nói với model "hồ sơ đang bị khoá" chỉ tổ khiến
nó bịa ra lý do hoặc đi thuyết phục người dùng mở khoá.

## 8. Kế hoạch theo giai đoạn

| GĐ | Nội dung | Rủi ro |
|---|---|---|
| **0** | **Vá ngay, không chờ vault**: `chmod 600` cho `config.json` + `project-config.json` + `senclaw.db*` khi tạo/ghi, theo mẫu `oauth/store.rs:177`. Chặn được tài khoản OS khác đọc key ngay hôm nay | **Rất thấp** |
| 1 | `src/vault/` — Argon2id + envelope + AEAD có AAD + `zeroize`; `vault.json`; unit test round-trip, sai mật khẩu, đổi mật khẩu | Thấp |
| 2 | Vòng đời phiên (unlock/lock/auto-lock), hook `SIGTERM`, REST + capability token (§6.5) | Trung bình — dễ sai ở phần miễn trừ loopback |
| 3 | Onboarding: `setup.rs` (CLI) **+** desktop UI; recovery code | Trung bình — dễ quên nhánh desktop |
| 4 | MCP server `senclaw-vault` (5 tool), permission gate cho `reveal`, audit log | Trung bình |
| 5 | Mã hoá từng trường lớp 1: `USER.md`, `oauth.json`, `apiKey`, `credentials_json`, `space_app_tokens` + migration hai chiều | **Cao** — migration dữ liệu thật |
| 6 | *(tuỳ chọn)* Lớp 2 mã hoá cả DB — prototype `sqlite3mc` + kiểm chứng `sqlite-vec`/FTS5 trước | **Cao** |

Giai đoạn 1–4 đã tự đứng được: có vault, có tool, có bí mật mới cất vào đó an
toàn. Giai đoạn 5 mới là phần chạm vào dữ liệu đang chạy — cần migration đảo
ngược được và backup tự động trước khi đổi.

## 9. Bẫy đã biết

- **Đừng dùng `Crypto::new_from_b64` cho mật khẩu người dùng** — SHA-256 một vòng
  (§2). Vault cần đường dẫn xuất riêng.
- **AAD rỗng cho phép hoán đổi ciphertext giữa các bản ghi.** Luôn gắn
  `version ‖ record_id ‖ label`.
- **Ghi tham số KDF ra file.** Đổi mặc định mà không ghi = khoá chết mọi vault cũ.
- **Route vault không được kế thừa miễn trừ loopback** (§6.5). Đây là lỗi dễ mắc
  nhất vì mọi route khác trong repo đều được miễn.
- **Mã hoá từng trường không bảo vệ nội dung đã vào FTS5** (§5.2).
- **Desktop không có TTY** → câu hỏi lần đầu trong `setup.rs` sẽ bị bỏ qua im
  lặng. Phải có nhánh UI.
- **`reveal=true` không có permission gate = một câu prompt-injection rút sạch
  vault.**
- **Không có recovery code thì quên mật khẩu là mất trắng.** Phải nói trước khi
  người dùng bấm xác nhận, không phải trong tài liệu.
- **Tuyệt đối không log mật khẩu, DEK, hay KEK** — kể cả ở mức `debug`. Nối vào
  [`src/safe_log.rs`](../src/safe_log.rs).
- **Đừng hứa quá.** Mã hoá at-rest không chống được malware chạy cùng quyền khi
  vault đang mở (§1). UI nên nói đúng phạm vi đó.

## 10. Nguồn

- [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) — tham số Argon2id
- [SQLCipher — Zetetic](https://www.zetetic.net/sqlcipher/)
- [SQLite3 Multiple Ciphers](https://utelle.github.io/SQLite3MultipleCiphers/) và [so sánh cipher](https://utelle.github.io/SQLite3MultipleCiphers/docs/ciphers/cipher_overview/)
- [rusqlite — feature `bundled-sqlcipher`](https://github.com/rusqlite/rusqlite?tab=readme-ov-file)
- [NSA CSI — MCP Security (05/2026)](https://www.nsa.gov/Portals/75/documents/Cybersecurity/CSI_MCP_SECURITY.pdf)
- [Red Hat — MCP: security risks and controls](https://www.redhat.com/en/blog/model-context-protocol-mcp-understanding-security-risks-and-controls)
- [Checkmarx — MCP Security Risks & Incidents (2026)](https://checkmarx.com/learn/mcp-security-risks-real-world-incidents-and-security-controls/)
- [Secure context passing thay vì truyền secret qua tham số tool](https://medium.com/@manuedavakandam/your-ai-agent-is-leaking-secrets-to-llms-when-calling-mcp-tools-fix-it-with-secure-context-passing-0da1ce072cd3)
