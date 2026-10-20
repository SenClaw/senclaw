# Plan: sen-link chỉ qua LAN, bỏ hub

Status: **P1 xong và đã commit, chưa push** (2026-10-20) · P2, P3 chưa làm · Repo: `senclaw` + `web-app` + `desktop`, nhánh `feat/slink-lan` trong `.worktrees/*-slink-lan` · Thay thế phần "Through the hub" của `../261009-2330-claude-code-cowork-sen-link/plan.md` (Round 2)
Tài liệu hiện hành: `docs/sen-link.md` · Code: `src/sen_link/`

## Outcome
Hai SenClaw cùng LAN tìm thấy nhau — tự động (mDNS), bằng nút **quét mạng**, hoặc **nhập tay IP hay tên DNS** — ghép nối bằng mã một lần, rồi giao việc cho nhau qua A2A như cũ. Không hub, không `publicUrl`, không phải mở cả daemon ra LAN.

## Non-goals
- Xuyên NAT / Internet (đó là việc của hub — bỏ hẳn).
- Đổi ngữ nghĩa A2A: map task ↔ kanban card, Triage mặc định, `autoRun`, assignee do operator đặt, MCP `link_*` — **giữ nguyên**.
- Streaming / push / đồng bộ board-memory (vẫn "Not yet").

## Hiện trạng (đọc từ code, 2026-10-20)
| # | Vấn đề khi dùng LAN hôm nay | Nguồn |
|---|---|---|
| 1 | Muốn **nhận** việc phải bind cả UI server ra ngoài loopback (`SENCLAW_UI_BIND_HOST`) → lộ `/api/*` (có token) **và** WS gateway (cùng host, `lib.rs:3134`) | `lib.rs:3474`, CLAUDE.md "Daemon network binding" |
| 2 | Phải tự gõ `publicUrl` mỗi bên + URL peer lúc ghép; DHCP đổi IP là hỏng `Peer.url` | `settings.rs`, `api.rs::pair` |
| 3 | HTTP trần: bearer `slk_…` và nội dung task đọc được trên LAN. Hub trước đây có AES-GCM từng cặp; bỏ hub mà không thay = hạ mức bảo mật. Task có thể là prompt chạy shell (`autoRun` + `claude-code`) | `client.rs::call`, `server.rs::rpc` |
| 4 | Hub: `relay.rs` 473 dòng + `RelayInfo` + `hubUrl` + chuỗi `senlink:` + vòng reconcile 30 s + nút "Pair via hub" ở 2 client + i18n ×2 + `via_hub` ở MCP. **Đã phát hành trong v0.2.0** → có thể có `peers.json` mang `relay` | `git tag --contains 5a8388c` = v0.2.0 |

Chỉ `sen_link/*` và `lib.rs:3460` dùng `sen_link::relay`. `clawhub::relay_client::RelayClient` và `util::crypto::Crypto` còn người dùng khác (kênh app) → **giữ**.

## Thiết kế

### D1. Listener LAN riêng (thay cho "bind UI ra ngoài")
- Khi `senLink.enabled`: mở listener axum thứ hai `listenHost:listenPort` (mặc định `0.0.0.0:18791`; 18788 UI, 18789 WS, 18790 browser-WS đã dùng).
- Router của listener này **chỉ có 3 route**: `GET /.well-known/agent-card.json`, `POST /a2a`, `POST /a2a/pair`. Không `/api`, không `/mcp`, không WS.
- `/api/sen-link/*` (quản lý) ở lại UI server trên loopback, sau token như mọi `/api`. Bật sen-link **không còn đòi** `SENCLAW_UI_BIND_HOST`.
- Agent Card `url` lấy từ header `Host` của request (đúng NIC khi nhiều card mạng) → bỏ `publicUrl`.

### D2. "Chỉ LAN" được cưỡng chế, không chỉ ghi chú
`fn is_lan_addr(IpAddr) -> bool`: cho phép loopback, RFC1918 (10/8, 172.16/12, 192.168/16), link-local (169.254/16, fe80::/10), ULA (fc00::/7, gồm dải IPv6 của Tailscale `fd7a:115c:a1e0::/48`) và **CGNAT 100.64.0.0/10 — dải IPv4 của Tailscale; đã chốt coi là LAN**; IPv4-mapped IPv6 xét theo phần v4. **Từ chối** mọi địa chỉ công cộng còn lại. 100.64/10 cũng là dải CGNAT của nhà mạng: "LAN" ở đây nghĩa là *mạng riêng*, không phải *mạng đáng tin* (xem Rủi ro) — ghép nối vẫn cần mã mời và (P3) khoá cặp.
- Vào: middleware trên listener dùng `ConnectInfo<SocketAddr>`, nguồn không phải LAN → 403 trước khi đọc body (port-forward qua router lộ IP công cộng → bị chặn).
- Ra: mọi kết nối đi ra (probe, pair, call, quét) dùng một `reqwest::Client` với resolver `LanOnly` — cùng khuôn `PublicOnly` ở `datagen/url_fetch.rs` nhưng đảo điều kiện: chỉ giữ địa chỉ LAN, rỗng thì lỗi. reqwest nối đúng địa chỉ đã kiểm (đóng cửa sổ DNS rebinding); IP literal không đi qua resolver nên được kiểm trước khi nối; `redirect(Policy::none())`. Phân loại địa chỉ viết một lần ở `sen_link/lan.rs`; xem `util::fetch_safety` có hàm dùng lại được không trước khi viết bảng mới.

### D3. Khám phá: mDNS / DNS-SD
- Dịch vụ `_senclaw-link._tcp.local.`, port = `listenPort`. TXT: `v=1`, `id=<daemon_id>`, `name=<tên hiển thị>`, `pair=1` khi có lời mời còn hạn. Không đưa bí mật hay thông tin task vào TXT.
- `daemon_id`: UUID sinh một lần, lưu `~/.senclaw/sen-link/identity.json` (0600).
- Crate đề xuất `mdns-sd` (thuần Rust, không cần Bonjour/Avahi, đa NIC) — **dependency mới**. Chỉ quảng bá trên interface có địa chỉ LAN (D2) và hỗ trợ multicast (cần kiểm `mdns-sd` chọn được interface như mong muốn). Interface Tailscale không chuyển multicast nên mDNS không thấy peer trong tailnet — tailnet dùng nhập tay (D3a).
- Bảng `discovered` trong bộ nhớ, hết hạn theo TTL: `{daemonId, name, addrs, port, pairable, lastSeen}`. `GET /api/sen-link/discovered`.
- `senLink.discoverable` (mặc định true): false = chỉ duyệt, không quảng bá.
- **Địa chỉ peer tự cập nhật**: peer lưu `daemon_id` + `addr`. Gọi lỗi và `addr` là IP literal → tra `discovered` theo `daemon_id` → thử địa chỉ mới → ghi lại. Hết bệnh DHCP. (`addr` kiểu tên DNS thì để DNS lo, không ghi đè.)
- mDNS là đường thụ động, tự chạy. Khi nó bị chặn (Wi-Fi khách cách ly client, VLAN, Docker bridge) hoặc không muốn dùng: **quét mạng** (D3b) hoặc **nhập tay** (D3a).
- Ba đường cho ra cùng một thứ — `addr` + `daemon_id` để ghép. `discovered` gộp mDNS và quét theo `daemon_id` (nguồn: `mdns`, `scan`).

### D3a. Nhập tay IP hoặc tên DNS
- Một ô, cú pháp `host[:port]`. `host` là IPv4, `[IPv6]` (cho phép zone: `[fe80::1%en0]`), hoặc tên (`mac-mini.local`, `nas.lan`, `mac-mini`, tên trong `/etc/hosts` hay DNS nội bộ). Port mặc định 18791. Dán cả `http://host:port/` vẫn được: bỏ scheme và `/` cuối. Từ chối `user@`, path/query, scheme khác `http(s)`, host rỗng, port 0 hoặc >65535. Hàm `parse_endpoint(&str) -> Result<Endpoint, String>` với `Host::Ip | Host::Name`.
- Phân giải bằng resolver của hệ điều hành (macOS có `.local` qua mDNSResponder; Linux cần `nss-mdns`), rồi lọc LAN như D2. Một tên bị từ chối **theo IP, không theo tên**: tên công cộng trỏ vào 192.168.x (split-horizon) vẫn dùng được; tên trỏ ra IP công cộng thì không.
- Lưu `Peer.addr` đúng như người dùng gõ: tên giữ là tên (theo DNS khi IP đổi); IP literal có thể được cập nhật từ `discovered` theo `daemon_id`. Địa chỉ kiểu tên không bao giờ bị ghi đè tự động.
- Nút **Kiểm tra** → `POST /api/sen-link/probe {addr}` → `{resolved: [ip…], reachable, card: {name, daemonId, pairable} | null, hidden, error}`. Hiện IP đã phân giải, tên peer và lý do thất bại (không phân giải được / ra ngoài LAN / từ chối kết nối / không phải sen-link) **trước** khi người dùng nhập mã. `hidden` = cổng mở nhưng card 404 (`discoverable=false`) — vẫn ghép được.
- **Tailscale**: nhập IP `100.x.y.z` hoặc tên MagicDNS (`macbook` hay `macbook.<tailnet>.ts.net`) — cả hai qua `LanOnly` là hợp lệ vì 100.64/10 là LAN. Quét mạng không phủ tailnet (interface Tailscale là /32, không sinh host nào, và 100.64/10 có ~4 triệu địa chỉ); đây là đường duy nhất cho tailnet.
- Agent Card thêm `capabilities.extensions[{uri: ".../sen-link/v1", params: {daemonId, pairable}}]` để probe và quét nhận ra sen-link và đọc `daemonId`. Cần kiểm với bản spec A2A đang dùng (`a2a.rs::PROTOCOL_VERSION`) rằng `extensions` hợp lệ ở chỗ này.

### D3b. Quét mạng
- **Chỉ chạy khi người dùng bấm**, không bao giờ tự chạy nền (mDNS lo phần tự động).
- Mục tiêu mặc định: với mỗi interface đang lên có IPv4 riêng + netmask → các host của subnet đó, trừ địa chỉ mạng, broadcast và chính mình. Tối đa **1024** host/lần; subnet rộng hơn (prefix < /22) thì cắt về /24 chứa địa chỉ của mình và nói rõ trên UI. Người dùng có thể nhập `cidr` (phải nằm trong dải LAN, ≤1024 host, nếu không 400) và `port` (peer đặt cổng khác). Chỉ IPv4 — không quét nổi một /64 IPv6; IPv6 dựa vào mDNS hoặc nhập tay.
- Mỗi host: TCP connect timeout 400 ms; nếu mở thì `GET /.well-known/agent-card.json` (timeout 1,5 s, thân ≤64 KiB). Chỉ nhận nếu là JSON có `provider.organization == "SenClaw"` và extension sen-link; phần còn lại bỏ im lặng (không ghi nội dung host lạ vào log). Song song 128 (semaphore): /24 ≈ 1 s, /22 toàn host chết ≈ 3 s.
- Job: `POST /api/sen-link/scan {cidr?, port?}` → 202 `{scanId, total}`; mỗi lần một job (đang chạy → 409); `GET /api/sen-link/scan/:id` → `{state: running|done|canceled, scanned, total, found: [...]}` (client poll); `POST /api/sen-link/scan/:id/cancel`. Kết quả gộp vào `discovered` (TTL 5 phút).
- Kết quả quét **chưa được xác thực** và tên giả được: người dùng chỉ thấy gợi ý (địa chỉ + tên + mã định danh ngắn). Xác thực nằm ở bước ghép — mã mời một lần + SPAKE2 (D5) chứng minh bên kia là chủ lời mời. Quét không ghép gì tự động.
- UI nói rõ việc sẽ làm ("thử kết nối cổng 18791 tới 254 địa chỉ trong 192.168.1.0/24"), vì quét TCP có thể làm IDS/EDR của mạng công ty báo động. macOS cũng hiện hộp thoại quyền Local Network ở lần quét đầu.
- `discoverable=false` ⇒ route card trả 404 ⇒ cả mDNS lẫn quét đều không thấy; vẫn ghép được bằng nhập tay.
- Liệt kê interface: `if-addrs` (chưa có trong `Cargo.lock`; `mdns-sd` có thể kéo theo — kiểm, và khai báo trực tiếp).

### D4. Ghép nối
1. B: `POST /api/sen-link/invites` → `{code, expiresAt}` (8 ký tự, một lần, 10 phút như cũ); `pair=1` bật trong TXT.
2. A: chọn B trong danh sách "Gần đây" (mDNS + kết quả quét), hoặc gõ IP / tên DNS rồi bấm Kiểm tra → nhập mã → `POST /api/sen-link/peers/pair {daemonId | addr, code}` (`addr` là IP hoặc DNS, D3a).
3. Bắt tay qua `POST /a2a/pair` trên listener của B (D5), hai bên lưu peer `{id, daemon_id, name, addr, outbound_token, inbound_token_hash, key, autoRun, assignee}`.

### D5. Kênh được niêm phong (thay AES-GCM của hub)
- Bắt tay **SPAKE2** (mã mời là mật khẩu) → HKDF → `pair_key` 32 B + xác nhận khoá hai chiều (sai mã là lộ ngay). PAKE nên kẻ nghe/đóng vai MITM **không dò mã offline** được; mã chỉ có ~40 bit nên đây là lý do không dùng DH + mã trộn HKDF.
- Mỗi lần gọi `/a2a` là một phong bì `{n, c, t}` (AES-256-GCM bằng `util::crypto::Crypto`) chứa `Frame::Req {id, ts, path, token, body}` — bearer token nằm **trong** phần mã hoá. Header `X-Sen-Link-Peer: <peer_id>` (không bí mật) chọn khoá. Phản hồi cũng được niêm phong.
- Dùng lại logic đã kiểm chứng của `relay.rs`: cửa sổ replay 2 phút + nhớ id (`Link::accept`), trần phản hồi 5 MiB, 8 request đồng thời, giải mã không panic. `relay.rs` bị xoá, nên chuyển các phần này sang `seal.rs` (lấy từ git history) cùng test.
- Chống đoán mã online: lời mời chết sau **3 lần xác nhận khoá sai** hoặc 1 lần thành công (hiện chỉ chết khi khớp; PAKE không biết sai/đúng đến lúc xác nhận).
- Crate mới: `spake2` (RustCrypto). `curve25519-dalek` và `hkdf` đã có trong `Cargo.lock` nhưng **chưa kiểm phiên bản có khớp** với `spake2` — kiểm khi thêm dependency; nếu lệch thì chọn bản `spake2` hợp với lockfile hiện có.
- Peer đã có `key` **bắt buộc** gọi bằng phong bì; không chấp nhận lại bearer trần (không hạ cấp được). Chỉ peer chưa có `key` mới dùng đường cũ.

### D6. Settings và dữ liệu
- `senLink`: **bỏ** `hubUrl`, `publicUrl`; **thêm** `listenHost` ("0.0.0.0"), `listenPort` (18791), `discoverable` (true). serde bỏ qua khoá lạ khi đọc; lần lưu sau tự xoá khoá cũ.
- `Peer`: bỏ `relay`; thêm `daemon_id`, `key`; `url` → `addr` (chuỗi `host:port` như người dùng gõ — IP hoặc tên DNS; giữ alias serde `url` để đọc file cũ, bỏ phần `http://` khi đọc). `PeerView`: bỏ `via_relay`, thêm `online`.
- **Di trú `peers.json` (v0.2.0 đã ship hub)**: lần mở đầu, sao lưu `peers.json.bak-<ts>`; peer chỉ có `relay` (không `url`) → xoá; peer có cả hai → giữ `url`, bỏ `relay`; lời mời có `relay` → xoá. `status.migrated: [tên…]` để UI báo một lần "đã bỏ N peer hub, hãy ghép lại qua LAN". Peer cũ (đã có `url`, chưa có `key`/`daemon_id`) vẫn gọi được ở chế độ cũ (bearer/HTTP) cho tới khi ghép lại; UI gắn nhãn "chưa mã hoá".

### D7. API
- Đổi: `POST /api/sen-link/invites` → `{code, expiresAt}`; `GET /api/sen-link/status` → `{settings, agentCard, peerCount, listening, discoverable, discovered}`; `POST /api/sen-link/peers/pair` nhận `{daemonId | addr, code}`.
- Thêm (D3a, D3b): `GET /api/sen-link/discovered`, `POST /api/sen-link/probe`, `POST /api/sen-link/scan`, `GET /api/sen-link/scan/:id`, `POST /api/sen-link/scan/:id/cancel`.
- Bỏ: trường `connect`, `warning`, `url` ở invite; `hubChannels`; `PairBody.connect`.
- MCP `link_peers`: `via_hub` → `online`. Các tool `link_send/task/tasks/cancel` không đổi. `docs/openapi-daemon.yaml` hiện không có route sen-link (0 khớp) nên không phải sinh lại.

### D8. Tên hiển thị: SLink
Đổi tên **chỉ ở giao diện**. Tên mã `sen-link` giữ nguyên ở mọi thứ máy đọc để không phá tương thích: đường dẫn `/api/sen-link/*`, khoá config `senLink`, thư mục `~/.senclaw/sen-link/`, module `sen_link`, MCP `senclaw-link` / `link_*`, khoá section `'sen-link'` của Settings (không đổi route), tên file.

Chuỗi người dùng thấy (kiểm kê bằng grep, 2026-10-20):
- Menu Settings: web `SettingsPage.tsx:130` (`label`), desktop `settings_screen.dart:69`.
- Tiêu đề: web `SenLinkSettings.tsx:186`; desktop `sen_link_section.dart:257,266`; ô board mặc định `sen_link_section.dart:87`.
- Thông báo `Could not load sen-link`, `Enable sen-link and save first.`: khoá i18n *chính là* chuỗi tiếng Anh, nên đổi **cả khoá lẫn giá trị** ở `vi.web.json`, `vi/cowork.dart` và test desktop (`find.text`). Lệch khoá thì bản tiếng Việt hiện tiếng Anh.
- Kanban: tên board mặc định (`settings.rs:37`), nhãn thẻ `sen-link` (`tasks.rs:104`), tiêu đề dự phòng "Task from sen-link", dòng mô tả "— from sen-link peer **x**".

Không đổi: Agent Card `name` (tên máy do người dùng đặt, mặc định "SenClaw"); mô tả tool cho model (`link_*`); tên file docs (chỉ thêm "SLink (tên mã `sen-link`)" ở đầu `docs/sen-link.md`).

**Board kanban cần xử lý riêng**: `ensure_board` và `ListTasks` tìm board theo `boardTitle`. Đổi mặc định sang "SLink" mà không làm gì thêm sẽ tạo board thứ hai và `ListTasks` không còn thấy task cũ. Đề xuất: giá trị lưu rỗng hoặc `sen-link` (mặc định cũ) được hiểu là `SLink`; khi chỉ có board tên `sen-link` thì **đổi tên board tại chỗ** (giữ nguyên thẻ). Thẻ mới mang nhãn `SLink`, thẻ cũ giữ nhãn cũ — không nơi nào lọc theo nhãn này (đã grep `src`, `web-app`, `desktop`, `app-claude`, `app-hub`). Xem câu hỏi mở 6.

## Danh sách xoá / sửa
| Nơi | Việc |
|---|---|
| `src/sen_link/relay.rs` | xoá (473 dòng); `seal.rs` mới nhận phần replay/giới hạn |
| `store.rs` | bỏ `RelayInfo`, `Invite.relay`, `relay_channels`, tham số `channel` của `redeem_invite`; thêm `daemon_id`, `key`, di trú |
| `server.rs` | `pair_with` bỏ `channel`; thêm bắt tay SPAKE2 + phong bì; bỏ nhánh "retry sau mất reply qua hub" (tính lại cho SPAKE2) |
| `client.rs` | bỏ `pair_via_hub`, nhánh `peer.relay`; thêm tra lại địa chỉ qua mDNS, kiểm tra `is_lan_addr` |
| `api.rs` | bỏ nhánh hub ở `invite`/`pair`, `reconcile`; thêm `discovered`, `probe`, `scan` |
| `lan.rs` (mới, P1) | `is_lan_addr`, `parse_endpoint`, resolver `LanOnly`, `reqwest::Client` dựng sẵn |
| `discovery.rs` (mới, P2) | mDNS quảng bá + duyệt, bảng `discovered` gộp theo `daemon_id` |
| `scan.rs` (mới, P2) | mở rộng CIDR/interface, probe TCP + card, job có tiến độ và huỷ |
| `settings.rs`, `tasks.rs`, `server.rs` | mặc định board "SLink" + đổi tên board cũ tại chỗ, nhãn thẻ `SLink`, chuỗi tiêu đề/mô tả thẻ (D8) |
| `settings.rs` | trường mới/bỏ trường cũ, validate `listenPort`, `listenHost` |
| `mod.rs` | `LinkState` bỏ `links`; thêm `discovery`; `router` tách làm LAN router + management router |
| `lib.rs` (≈3445–3485) | bỏ `relay::start`; spawn listener LAN + discovery khi `enabled`, dừng khi tắt |
| `mcp/link_server.rs` | `via_hub` → `online`, sửa doc comment |
| web-app `SenLinkSettings.tsx`, `coworkApi.ts`, `i18n/vi.web.json` | bỏ ô Hub URL, ô chuỗi `senlink:`, nút "Pair via hub", tag `hub`; thêm hộp thoại "Thêm peer": danh sách "Gần đây" + nút **Quét mạng** (tiến độ, huỷ), ô nhập IP/DNS + nút Kiểm tra, ô mã; nhãn online/chưa mã hoá. Đổi tên hiển thị SLink (D8) |
| web `SettingsPage.tsx`, desktop `settings_screen.dart` | nhãn menu `sen-link` → `SLink` (giữ khoá section) |
| desktop `sen_link_section.dart`, `core/i18n/vi/cowork.dart`, `test/cowork_sections_test.dart` | tương tự |
| desktop `macos/Runner/Info.plist` | `NSLocalNetworkUsageDescription` + `NSBonjourServices` = `_senclaw-link._tcp` |
| `docs/sen-link.md`, CLAUDE.md (mục Space-App MCP naming không đổi) | viết lại phần Enable/pair, bỏ "Through the hub", thêm listener/mDNS/niêm phong |
| `senclaw-hub-connector` | không còn dùng cho sen-link; repo hub không đụng |

## Giai đoạn (mỗi giai đoạn tự đứng được, có thể dừng sau bất kỳ giai đoạn nào)
| # | Giai đoạn | Kết quả | Phụ thuộc |
|---|---|---|---|
| P1 ✅ | Bỏ hub + listener LAN riêng + `lan.rs` (`is_lan_addr`, `parse_endpoint`, `LanOnly`) + **nhập tay IP/DNS + `probe`** + settings/di trú + 2 client + đổi tên hiển thị SLink (D8) + docs | LAN (cả Tailscale) chạy bằng IP hoặc tên DNS gõ tay, bearer/HTTP như cũ; không còn phải bind UI ra ngoài | — |
| P2 | Tìm tự động: mDNS (quảng bá, duyệt, tự cập nhật địa chỉ IP) + **quét mạng** (job, tiến độ, huỷ) + danh sách "Gần đây" + Info.plist + `if-addrs` | Không cần gõ IP trong LAN thường | P1 |
| P3 | SPAKE2 + phong bì + replay (`seal.rs`) + khoá lời mời theo số lần sai | Kênh LAN mã hoá; peer cũ hiện "chưa mã hoá" | P1 (P2 không bắt buộc) |

## Kiểm thử và nghiệm thu
Unit (`#[cfg(test)]` cạnh code):
- `is_lan_addr`: 10.x, 172.16–31, 192.168, 169.254, 100.64.0.1, 100.127.255.255, fc00::/7 (gồm `fd7a:115c:a1e0::1`), fe80::/10, 127/8, ::1 → true; 8.8.8.8, 172.32.0.1, 100.63.255.255, 100.128.0.1, `::ffff:8.8.8.8` → false.
- Router LAN có đúng 3 route: `/api/...`, `/mcp/...` → 404; nguồn công cộng → 403.
- `parse_endpoint`: `192.168.1.20`, `192.168.1.20:18791`, `[fe80::1%en0]:18791`, `mac-mini.local`, `100.101.102.103`, `http://nas.lan:18791/` → ok (port mặc định 18791, bỏ scheme và `/`); `user@h`, `h/path`, `ftp://h`, `h:0`, `h:70000`, rỗng → lỗi.
- `LanOnly`: tên → {8.8.8.8, 192.168.1.5} chỉ còn địa chỉ LAN; chỉ có địa chỉ công cộng → lỗi; IP literal công cộng bị từ chối trước khi nối; server trả 302 sang địa chỉ công cộng → không đi theo.
- `probe`: đúng lý do cho từng ca — không phân giải được / ra ngoài LAN / từ chối kết nối / không phải sen-link / card ẩn (`hidden`).
- Quét: mở rộng CIDR bỏ địa chỉ mạng, broadcast và chính mình; `/16` và `8.8.8.0/24` bị từ chối; interface `/23` cắt về `/24`; interface `/32` (Tailscale) không sinh host; trên 3 listener loopback (sen-link thật, HTTP lạ trả 200, cổng đóng) chỉ thấy cái thật; host nhận kết nối mà không trả lời bị timeout chặn; huỷ giữa chừng dừng sớm; job thứ hai khi đang chạy → 409.
- Board: `boardTitle` rỗng hoặc `sen-link` + board cũ `sen-link` → đổi tên tại chỗ, thẻ và `ListTasks` còn nguyên; không tạo board thứ hai.
- Di trú: peer chỉ-relay bị xoá, có bản `.bak`, peer có `url` giữ; `senLink` cũ có `hubUrl` vẫn đọc được.
- SPAKE2: sai mã → xác nhận khoá hỏng; 3 lần sai → lời mời chết; phong bì sai khoá/cũ/lặp bị từ chối (chuyển test replay của `relay.rs`).
E2E (hai daemon cô lập, `SENCLAW_HOME`+`HOME`+cwd scratch, cổng lạ, Seatbelt chặn 18788/18789/18790 **và 18791**):
- ghép bằng `addr` + mã → giao task → `claude-code`/persona chạy → artifacts về; hai chiều. Ghép cả bằng IP literal lẫn bằng tên DNS (`localhost` → 127.0.0.1 và ::1); quét `127.0.0.0/30` (loopback nằm trong `is_lan_addr`, nên E2E chạy được trên một máy).
- **Chưa thử được hai máy thật trong môi trường này**: cần một lần thử tay trên hai máy cho mDNS, quét, tên `.local` và một tailnet thật (IP `100.x` + tên MagicDNS).
- Giao diện: ảnh chụp web và desktop sau khi đổi tên — menu, tiêu đề, thông báo (cả tiếng Việt) và board Kanban đều ghi SLink; không còn `sen-link` hiển thị (`grep` chuỗi hiển thị).
- listener LAN: `curl /api/sen-link/status` trên cổng LAN → 404; UI server vẫn chỉ loopback khi sen-link bật.
- P2: `dns-sd -B _senclaw-link._tcp` thấy cả hai; đổi cổng/IP rồi gọi lại → tự tìm địa chỉ mới.
- `cargo test --lib` toàn bộ + `flutter test` (cowork_sections) + build web-app.

## Rủi ro
- **LAN ≠ đáng tin** (Wi-Fi quán cà phê là /24 riêng). Giảm thiểu: `enabled` mặc định false, mã một lần, Triage mặc định, kênh mã hoá (P3), `listenHost` chọn được interface. Xem câu hỏi mở 3.
- macOS 15+ hỏi quyền Local Network; thiếu `NSBonjourServices` thì mDNS im lặng không thấy ai — phải thử trên bản app thật, không chỉ `cargo run`.
- mDNS vô dụng trong Docker bridge / mạng cách ly client → luôn có đường `addr` tay; bản Docker cần `--network host` (ghi vào docs).
- Quét TCP trên mạng có quản lý có thể bị IDS/EDR hay chính sách chặn: chỉ chạy khi bấm, tối đa 1024 host, song song 128, UI báo trước. Wi-Fi cách ly client (AP isolation) làm cả quét lẫn mDNS không thấy ai — UI nói "cổng không mở hoặc bị cách ly" thay vì đoán nguyên nhân.
- Tên `.local` đi qua resolver hệ điều hành: macOS có sẵn, Linux cần `nss-mdns`, Windows tuỳ bản. Không phân giải được `.local` không phải lỗi sen-link — `probe` phải nói đúng điều đó.
- 100.64/10 là LAN (đã chốt) nhưng cũng là CGNAT của nhà mạng: coi nó là *mạng riêng*, không phải *mạng đáng tin*. Cũng là lý do nên làm P3.
- `mdns-sd`, `if-addrs` và `spake2` là dependency mới: cần duyệt giấy phép + kích thước binary trước khi gộp.

## Kết quả P1 (2026-10-20)

Đã làm đủ phạm vi P1 ở cả ba repo (chưa commit): bỏ hub (`relay.rs` xoá), listener LAN riêng + chặn nguồn ngoài LAN, `lan.rs` (`is_lan_addr`, `parse_endpoint`, resolver `LanOnly`, client dựng một lần), nhập tay IP/DNS + `probe`, settings mới + di trú `peers.json`, đổi tên hiển thị SLink (kể cả board Kanban đổi tên tại chỗ), web-app + desktop, docs, mục SLink trong `CLAUDE.md`.

Kiểm chứng:
- `cargo test --lib`: 3334 qua, 0 lỗi (47 test `sen_link`: LAN, địa chỉ gõ tay, listener theo settings, hai daemon ghép bằng tên và bằng IP rồi giao việc hai chiều, probe, di trú, board cũ).
- Hai daemon thật cô lập (Seatbelt chặn 18788–18791, `SENCLAW_HOME`/`HOME`/cwd scratch): di trú `peers.json` có bản `.bak`; UI server chỉ trả trang web (POST `/a2a` → 405), cổng LAN chỉ có 3 route (`/api/*` → 404); probe/pair bằng tên và IP; giao việc A→B và B→A; ghép nối còn nguyên sau khi khởi động lại; tắt SLink đóng socket và cả kết nối keep-alive; port bận báo `listenError`, tự hồi phục. Giao diện web chụp thật: menu/tiêu đề SLink, dòng "Listening on", địa chỉ máy này, Kiểm tra.
- `npm run build` xanh; `flutter analyze` sạch, `flutter test` 508 qua.

Lệch so với thiết kế (đã chọn, cần biết):
1. `if-addrs` được kéo từ P2 sang P1: hộp thoại mời cần hiện địa chỉ của máy này để bên kia gõ.
2. Scope id IPv6 (`fe80::1%en0`) **không hỗ trợ** (kiểu URL không mang được); báo lỗi rõ.
3. `online` ở `PeerView`/MCP chưa có (cần khám phá, P2): `link_peers` trả `addr` thay `via_hub`. `pair` chỉ nhận `addr` (dạng `daemonId` đến cùng P2).
4. Peer ghép trực tiếp từ v0.2.0 giữ địa chỉ cổng UI cũ (`…:18788`), nên chỉ gọi được **một chiều** (máy mới gọi máy cũ, nếu UI máy cũ còn mở); đánh dấu `legacy`, cần ghép lại. Docs nói rõ, thay cho câu "vẫn gọi được" trong bản thiết kế.
5. Thêm: ghép lại cùng một daemon (cùng `daemonId`) thay bản ghi cũ, giữ id (task nằm dưới id đó), `autoRun`, assignee; không sinh bản sao.
6. Thêm (tìm ra khi chạy thật): listener đóng êm bằng graceful shutdown. Bản đầu chỉ `abort` task, kết nối keep-alive của peer sống tiếp và trả 404 sau khi SLink đã tắt.
7. `probe` nhận ra daemon ẩn (`discoverable: false`) bằng cách POST `{}` vào `/a2a/pair` và mong 422 — cửa ghép nối vẫn mở khi thẻ ẩn.
8. Vì `vi.web.json` từng ghi đè khoá `Hub URL` (dùng chung với trang Marketplace), xoá nó trả `Hub URL` của Marketplace về bản dịch gốc "URL Hub".

Chưa kiểm chứng được: hai máy thật trong LAN thật, tailnet thật (IP `100.x`, tên MagicDNS), tên `.local`; cửa sổ app desktop thật (chỉ widget test); tiếng Việt trên giao diện web chỉ kiểm bằng script (mọi chuỗi đều có bản dịch), không xem bằng mắt.

Còn lại: P2 (mDNS + quét mạng + danh sách "Gần đây" + Info.plist), P3 (SPAKE2 + phong bì). Cho tới P3, kênh là HTTP + bearer trên LAN và `/a2a/pair` không giới hạn số lần thử mã.

## Câu hỏi mở
1. **Mức bảo mật kênh**: làm P3 (khuyến nghị) hay dừng ở HTTP + bearer cho "LAN tin cậy"? Dừng ở P2 vẫn đúng yêu cầu "chỉ LAN, không hub" nhưng task có thể mang prompt chạy shell, đọc được trên LAN.
2. ~~Tailscale / 100.64.0.0/10~~ — **đã chốt: coi là LAN** (D2, D3a).
3. **Chỉ mở cổng khi cần?** (nghe/quảng bá chỉ khi có peer hoặc lời mời còn hạn → mặc định không lộ gì). Khuyến nghị bật, tốn thêm một vòng quản lý listener.
4. Cổng mặc định 18791 ổn không (18790 đã là browser-WS)?
5. **Quét có tự chạy khi mở hộp thoại "Thêm peer" không?** Khuyến nghị không: mDNS đã lo phần tự động; quét là hành động chủ động và có thể làm IDS báo động.
6. **Board kanban "sen-link" cũ**: đổi tên tại chỗ thành "SLink" (khuyến nghị, giữ nguyên thẻ và lịch sử), hay để board cũ nguyên tên và chỉ máy mới có board "SLink"? Cách sau làm `ListTasks` chỉ thấy task của board đang cấu hình.
