# Nghiên cứu: Space App quét bảo mật máy chủ & website

> Trạng thái: **nghiên cứu + thiết kế**, chưa viết code. Ngày 2026-07-31.
> Mọi con số trong tài liệu này đều đo thật trên máy hiện tại, không suy đoán.
> Phần kiểm chứng gốc: xem mục "Phụ lục A — nhật ký kiểm chứng".

## 0. Quan hệ với `sentinel` — hai app khác nhau, đừng gộp

Trong repo đã có [docs/sentinel-app-design.md](sentinel-app-design.md) (cùng ngày,
cũng chưa implement). **Hai app không trùng nhau**, nhưng ban đầu cùng nhắm cổng 4680
nên tài liệu này **nhường 4680 cho sentinel và lấy 4690**.

| | `sentinel` (`sen_`) | `secscan` — tài liệu này (`sec_`) |
|---|---|---|
| Nhìn vào đâu | **Bên trong SenClaw** | **Hạ tầng bên ngoài** |
| Câu hỏi | "Agent có bị lạm dụng / cấu hình có bị đổi lén không?" | "Website và máy chủ của tôi có lỗ hổng gì?" |
| Dữ liệu | bảng của daemon: `tool_executions`, `groups.allowed_tools`, `tool_rules`, hooks | HTTP/TLS/DNS của mục tiêu, gói OS qua SSH |
| Vai | phát hiện & điều tra (detective/forensic) | đánh giá tư thế (posture assessment) |

Bổ sung cho nhau: sentinel canh **người canh cửa**, secscan canh **cái cửa**.
Cả hai đều dùng lại khuôn Space App và đều **không** có lớp khai thác.

Có một chỗ giao nhau đáng khai thác: lỗi phơi nhiễm ở §1 dưới đây là **lỗi của chính
SenClaw** — secscan tìm ra nó bằng phép kiểm hạ tầng thông thường, còn sentinel thì
nên coi thay đổi cấu hình dẫn tới nó là một sự kiện cần ghi vết.

## 1. Vì sao nên làm, và bằng chứng

Chạy vài phép thử thụ động vào chính SenClaw trên máy này đã ra lỗi thật:

| Phát hiện | Mức | Bằng chứng |
|---|---|---|
| Space App bind `0.0.0.0`, không xác thực → cả LAN vào được | **HIGH** | `http://192.168.0.101:4390/` (CRM, dữ liệu khách hàng) trả `200` từ IP LAN |
| Toàn bộ API daemon trả `Access-Control-Allow-Origin: *`, không auth | **HIGH** | `/api/groups`, `/api/llm-config`, `/api/space/apps` đều `ACAO=*` |
| ssh-manager chạy 2 tiến trình mồ côi, cổng ngẫu nhiên | MEDIUM | PID 24327 `*:57426`, PID 32727 `*:64863` |

Daemon (18788/18789) thì làm đúng — bind `127.0.0.1`, LAN không với tới.

> ⚠️ Tôi **chưa** kiểm tra `/api/llm-config` có lộ API key không: thao tác đọc giá
> trị bị chặn và tôi không lách. Anh tự mở xem; nếu có key thật thì mức lên
> **CRITICAL** và cần vá trước mọi việc khác.

Đây chính là luận điểm sản phẩm: **một scanner chỉ chạy phép thử thụ động, không
gửi một payload tấn công nào, vẫn ra kết quả đáng giá.** Không cần OpenVAS, không
cần Nessus, không cần quyền root.

## 2. Phạm vi: làm gì và KHÔNG làm gì

**Làm** — đánh giá tư thế bảo mật (posture assessment) của hạ tầng **mình sở hữu**:

- Website: TLS/chứng thư, security header, cờ cookie, lộ thông tin phiên bản, CORS sai, DNS/email posture (SPF/DKIM/DMARC/CAA/DNSSEC), thư viện JS lỗi thời.
- Máy chủ: cấu hình SSH, cổng đang mở, CVE của gói hệ điều hành, tường lửa, quyền tệp, dịch vụ chạy bằng root.
- Theo dõi trôi cấu hình theo thời gian + cảnh báo khi có phát hiện mới.

**Không làm** — và đây là ranh giới cứng, không phải tùy chọn:

- Không khai thác lỗ hổng, không payload phá hoại, không brute-force, không DoS.
- Không quét mục tiêu chưa chứng minh quyền sở hữu (§6).
- Không lưu mật khẩu/khoá riêng dạng plaintext.
- Không "tự động vá" — chỉ đề xuất, người vận hành quyết định.

### 2.1 Cơ sở pháp lý — luật đã ĐỔI cách đây 30 ngày

⚠️ **Luật An ninh mạng 24/2018/QH14 đã bị bãi bỏ từ 01/7/2026**, thay bằng
**Luật An ninh mạng số 116/2025/QH15** (thông qua 10/12/2025, 8 chương 45 điều).
Điều 44.2 bãi bỏ **cả** Luật 24/2018 **và** Luật An toàn thông tin mạng 86/2015.
Mọi tài liệu viết trước 7/2026 dẫn Luật 24/2018 đều đã lỗi thời.

**Điều 15.2.a của luật mới đặt NGHĨA VỤ chủ hệ thống phải tự kiểm tra**: *"Kiểm tra
an ninh mạng nhằm phát hiện, loại bỏ mã độc … khắc phục điểm yếu, lỗ hổng bảo mật"*;
Điều 26.2.đ thêm nghĩa vụ đánh giá rủi ro định kỳ. → Quét hệ thống **của chính mình
là việc luật bắt phải làm**. Đây là chỗ đứng hợp pháp của app.

Ngược lại, **Điều 7.5** cấm *"Xâm nhập trái phép vào mạng viễn thông, mạng máy tính,
hệ thống thông tin … cơ sở dữ liệu, phương tiện điện tử của người khác"* — rộng hơn
bản 2018, nay phủ cả hệ thống điều khiển (ICS/OT) và cơ sở dữ liệu.

Vì vậy ranh giới "chỉ quét cái mình sở hữu" ở §6 **không phải là lựa chọn thiết kế
cho đẹp — nó là ranh giới giữa nghĩa vụ pháp lý và hành vi bị cấm.**

### 2.2 Ba con số hình sự để lấy làm mốc kỹ thuật

Bộ luật Hình sự (các điều 285–289 **không** bị sửa bởi Luật 86/2025):

- **Điều 287** — làm tê liệt mạng **từ 30 phút đến dưới 24 giờ**, *hoặc* **3 đến dưới
  10 lần trong 24 giờ**, là đã đủ cấu thành — **không cần thiệt hại tài chính**.
  → Đây là con số phải thiết kế bộ giới hạn tốc độ để tránh, và nên đặt thấp hơn nó
  một bậc độ lớn. "Lạm dụng quyền quản trị mạng" là tình tiết tăng nặng khoản 2 —
  đáng lưu ý cho chế độ quét có đăng nhập.
- **Điều 289** — cố ý vượt qua cảnh báo/mã truy cập/tường lửa để xâm nhập trái phép
  mạng **của người khác** *và* có ít nhất một hậu quả (chiếm quyền, can thiệp chức
  năng, lấy/sửa/huỷ dữ liệu, **hoặc sử dụng trái phép dịch vụ**): phạt 50–300 triệu
  hoặc **1–5 năm tù**; lên 7–12 năm nếu chạm hạ tầng quốc gia/ngân hàng/điện lực.
  Quét hệ thống của **chính mình** nằm ngoài điều này.
- **Điều 288.1.b** — mua bán/công khai thông tin riêng hợp pháp của tổ chức khi chưa
  được phép, gây *"dư luận xấu làm giảm uy tín"* — **không có ngưỡng tài chính**.
  → Ràng buộc trực tiếp lên việc công bố báo cáo có nêu tên tổ chức.

**Nghị quyết 08/2021/NQ-UBTVQH15** giải thích *"lấy cắp dữ liệu"* bao gồm cả
*"nghe, đọc, ghi chép, chụp ảnh, ghi âm, ghi hình"* dữ liệu chứa bí mật kinh doanh.
→ **Chỉ cần quan sát là đủ.** Nghĩa là **che/rút gọn dữ liệu trong kết quả quét là
biện pháp kiểm soát trách nhiệm hình sự, không phải chuyện vệ sinh code.**

### 2.3 ⚠️ Nếu định thương mại hoá: cần giấy phép

Đây là hệ quả lớn nhất về mặt kinh doanh và tôi không thấy nó trước khi tra:

**Điều 28.1.b** của Luật 116/2025 xếp *"Sản phẩm kiểm tra, đánh giá an ninh mạng"*
vào nhóm sản phẩm an ninh mạng, và **Điều 29.1**: *"Doanh nghiệp kinh doanh sản phẩm,
dịch vụ an ninh mạng **phải có giấy phép** kinh doanh sản phẩm, dịch vụ an ninh mạng."*

Dự thảo nghị định của Bộ Công an mô tả nhóm này là *"Rà quét, kiểm tra, phân tích cấu
hình … phát hiện lỗ hổng, điểm yếu; đưa ra đánh giá rủi ro an ninh mạng"* — đúng từng
chữ một cái scanner. Điều kiện dự thảo gồm ≥5 nhân sự kỹ thuật thường trú và **người
đại diện pháp luật mang quốc tịch Việt Nam**. Cơ quan quản lý đã chuyển từ Bộ TT&TT
sang **Bộ Công an (A05)** từ 28/02/2025. Sản phẩm có trước 01/7/2026 được 12 tháng
để tuân thủ (Điều 45.3).

→ **Dùng nội bộ cho hạ tầng của chính mình thì không vướng.** Bán ra ngoài, hoặc phát
hành trên hub như một dịch vụ, thì phải hỏi luật sư trước. Việt Nam **không có** miễn
trừ cho nghiên cứu bảo mật và **không có** safe harbour cho pentest có uỷ quyền —
lá chắn duy nhất là sự đồng ý của chủ hệ thống và yếu tố `của người khác`.

Về dữ liệu cá nhân: Luật BVDLCN **91/2025/QH15** (hiệu lực 01/01/2026) +
**Nghị định 356/2025/NĐ-CP** đã **thay** Nghị định 13/2023. Chuyển dữ liệu ra nước
ngoài trái phép phạt tới **5% doanh thu năm trước**; báo cáo sự cố trong 72 giờ.
→ Scanner dạng SaaS lưu kết quả ở nước ngoài có rủi ro thật ở đây.

## 3. Kiến trúc

Theo đúng khuôn Space App hiện hành (bản mẫu gần nhất: `apps/autotest`).

```
apps/secscan/                      cổng 4690  (4680 đã bị sentinel giữ chỗ — xem §0)
├── Cargo.toml                     edition 2021, thêm "apps/secscan" vào workspace members
├── senclaw-manifest.json          mcp.name = "secscan-mcp", healthPath /api/status
├── src/
│   ├── main.rs                    bootstrap axum + ServeDir (sao nguyên autotest)
│   ├── api.rs                     AppState, api_router, các *_value helper
│   ├── db.rs                      SQLite, const SCHEMA + fn migrate()
│   ├── mcp.rs                     JSON-RPC tay, tools_list() + call_tool()
│   ├── llm.rs                     SpaceClient — phân loại & giải thích
│   ├── scope.rs                   ★ sổ tài sản + xác minh quyền sở hữu
│   ├── web/                       ★ đầu dò website (header, cookie, CORS, redirect)
│   │   ├── headers.rs  tls.rs  cookies.rs  cors.rs  paths.rs  fingerprint.rs
│   ├── dns.rs                     ★ SPF/DKIM/DMARC/CAA/DNSSEC
│   ├── host.rs                    ★ audit máy chủ qua SSH (chỉ đọc)
│   ├── vuln.rs                    ★ OSV + KEV + EPSS
│   ├── score.rs                   ★ chấm điểm & xếp hạng
│   └── sched.rs                   vòng lặp nền quét định kỳ
├── web/                           React 19 + antd 6 + Vite 8 (không Tailwind)
├── skills/secscan-audit/SKILL.md
└── scripts/pack.sh
```

★ = phần riêng của app này; còn lại sao chép khuôn có sẵn.

Manifest (khớp đúng khuôn daemon đang đọc — `runtime.port` cố định, `autoRegister`
bắt buộc `true`, `mcp.transport` là `"http"` dù đường dẫn kết thúc bằng `/sse`):

```json
{
  "id": "secscan",
  "name": "Quét Bảo Mật",
  "icon": "🛡️",
  "runtime": { "kind": "server", "start": "./secscan",
               "healthPath": "/api/status", "port": 4690 },
  "integration": { "type": "iframe", "url": "/" },
  "bridge": { "postMessage": true,
              "capabilities": ["space.rest", "llm.request"] },
  "mcp": { "name": "secscan-mcp", "transport": "http",
           "path": "/api/mcp/sse", "autoRegister": true },
  "skills": [{ "name": "secscan-audit", "path": "skills/secscan-audit",
               "triggers": ["quét bảo mật", "kiểm tra bảo mật", "lỗ hổng",
                            "an toàn website", "audit server"] }]
}
```

Nhớ thêm `"apps/secscan"` vào `members` của `Cargo.toml` gốc, nếu không
`cargo build -p secscan` sẽ không thấy crate.

### 3.1 Ba lớp, tách theo mức độ xâm nhập

| Lớp | Gửi gì tới mục tiêu | Chạy được trên production? | Cần quyền |
|---|---|---|---|
| **L1 — Thụ động** | 1 GET, vài truy vấn DNS, 1 bắt tay TLS | Có, vô hại | Không |
| **L2 — Chủ động nhẹ** | vài chục request hợp lệ (đường dẫn phổ biến, thử Origin, thử redirect) | Có, nhưng phải giới hạn tốc độ | Xác minh sở hữu |
| **L3 — Nội bộ** | SSH đọc cấu hình, liệt kê gói | Có | Tài khoản SSH chỉ-đọc |

Không có lớp "tấn công". Đó là lựa chọn thiết kế, không phải giai đoạn chưa làm.

### 3.2 Luồng dữ liệu CVE — không cần cơ sở dữ liệu cục bộ

Đây là phát hiện quan trọng nhất của đợt nghiên cứu:

```
SSH: dpkg-query -W -f='${Package} ${Version}\n'     (hoặc rpm -qa)
  ↓  gom lô 100
POST https://api.osv.dev/v1/querybatch              ← keyless, 100 truy vấn/0.76s
  ↓  ra danh sách CVE cho từng gói
ghép EPSS (api.first.org)  +  CISA KEV (1656 CVE, cập nhật 2026-07-29)
  ↓
xếp hạng: KEV trước → EPSS cao → CVSS
```

Đo thật: `openssl 3.0.2-0ubuntu1.10` trên Ubuntu 22.04 → **53 CVE**;
`curl 7.88.1-10+deb12u5` trên Debian 12 → **35 CVE**. Máy 600 gói ≈ 6 request ≈ ~5 giây.

Nghĩa là **không cần cài Trivy/OpenVAS, không cần đồng bộ NVD, không cần API key.**

#### Vì sao chọn OSV chứ không phải NVD — lý do 2026

Từ **2026-04-15** NIST chỉ còn làm giàu dữ liệu cho CVE thuộc KEV / phần mềm liên
bang / EO-14028. Đo trên mẫu CVE công bố 1–20/07/2026: **chỉ 42% có `configurations`
(CPE)** và **17% có CVSS do NVD tự chấm**. Trạng thái `Awaiting Analysis` và
`Undergoing Analysis` giờ **bằng 0** — tồn đọng không được xử lý mà bị đổi nhãn
sang `Deferred` (41 747 mục).

→ Scanner khớp lỗ hổng bằng CPE trên NVD sẽ **âm thầm bỏ sót phần lớn CVE 2026**.
OSV khớp theo *khoảng phiên bản của gói*, không cần CPE, nên không dính vấn đề này.
NVD chỉ nên dùng bổ trợ (lấp CVSS), không làm lõi khớp.

#### Ba nguồn, ba cách lấy

| Nguồn | Cách dùng | Ghi chú đã kiểm |
|---|---|---|
| **OSV.dev** | `POST /v1/querybatch`, không auth, không giới hạn công bố | 45 hệ sinh thái, gồm `Debian:12`, `Ubuntu:22.04`, `Alpine:v3.19`, `Red Hat`, `SUSE`. Dump khối `Debian/all.zip` 68.6 MB, `modified_id.csv` để đồng bộ tăng dần |
| **CISA KEV** | tải JSON 1 lần/ngày | 1656 mục, `catalogVersion 2026.07.29` — khớp đúng số tôi đo |
| **EPSS v5** | bulk CSV 1 lần/ngày, **đừng gọi API cho khối lượng lớn** | `epss_scores-current.csv.gz` 2.51 MB, 354 454 CVE |

Ba bẫy kỹ thuật ở tầng này:

- **OSV giới hạn phản hồi 32 MiB trên HTTP/1.1**, không giới hạn trên HTTP/2 →
  bật ALPN/HTTP2 cho `reqwest`, nếu không lô lớn sẽ đứt.
- **EPSS CSV có dòng `#model_version:v2026.06.15,...` TRƯỚC dòng header** — đọc CSV
  ngây thơ sẽ lấy nhầm dòng đó làm header. Bỏ dòng 1. Và URL `-current` **301
  redirect** sang tệp có ngày → phải bật follow-redirect.
- EPSS trả `epss`/`percentile` dạng **chuỗi**, không phải số — parse cho đúng.

#### Vá backport — nguồn dương-tính-giả lớn nhất khi quét CVE máy chủ

Đây là chỗ **phải hiểu trước khi viết một dòng code nào** của `vuln.rs`.

Các bản phân phối doanh nghiệp **backport** bản vá vào một phiên bản thượng nguồn
đã đóng băng. Chuỗi phiên bản thượng nguồn **không đổi**; chỉ số hiệu chỉnh của distro đổi.

Ubuntu phát hành `openssl 3.0.2-0ubuntu1.15`. Thượng nguồn 3.0.2 dính hàng chục CVE
đã vá ở 3.0.8/3.0.9. Scanner nào đọc ra "3.0.2", tra NVD, so với "fixed in 3.0.8" sẽ
báo **toàn bộ số đó là chưa vá — và sai toàn bộ**, vì Canonical đã vá bên trong
`-0ubuntu1.15`. Tương tự `1.2.3-4+deb12u3` (Debian) và `1.2.3-4.el9_4.2` (RHEL).

> Câu hỏi đúng **không phải** "phiên bản này có nằm trong khoảng dính lỗi không?"
> mà là **"máy này có thiếu bản dựng gói đã vá CVE đó của distro không?"**

Kéo theo hai ràng buộc:

- **Phải dùng dữ liệu gốc của distro** (OSV / OVAL / security tracker), **không dùng
  CPE của NVD** — CPE mô tả sản phẩm của nhà cung cấp, không mô tả gói của distro, và
  **về bản chất không mang thông tin backport**.
- **Phải so sánh phiên bản theo luật của distro**, không phải semver: `dpkg --compare-versions`
  hoặc `rpmvercmp` — chúng xử lý `epoch` và thứ tự dấu ngã (`~rc1 < release`).
  **Semver thường sẽ xếp sai thứ tự mà không báo lỗi.**

Khi đọc danh sách gói, lấy cả **gói nguồn**:
`dpkg-query -W -f='${binary:Package}\t${Version}\t${source:Package}\t${source:Version}\n'`
— dữ liệu bảo mật Debian/Ubuntu khoá theo **gói nguồn** (`libssl3` ← nguồn `openssl`).
Với RPM phải lấy đủ **NEVRA kể cả epoch**; **bỏ `%{RELEASE}` là lỗi kinh điển** —
`openssl-3.0.7-27.el9` và `openssl-3.0.7-28.el9_4` chỉ khác nhau ở release, mà bản vá
nằm đúng chỗ đó.

Nên nhận **VEX** để tắt bớt phát hiện, và nên **phát VEX cho kết quả của chính mình**.

#### KEV: hạn xử lý đã đổi nghĩa (BOD 26-04)

**BOD 22-01 đã bị thu hồi**, thay bằng [BOD 26-04](https://www.cisa.gov/news-events/directives/bod-26-04-prioritizing-security-updates-based-risk)
ban hành 2026-06-10. Hạn khắc phục không còn theo mức CVSS phẳng mà theo **rủi ro**,
dựa trên bốn biến SSVC: mức phơi nhiễm công khai, có trong KEV không, tự động hoá
được không, tác động kỹ thuật. Tổ hợp xấu nhất = **3 ngày**.

→ Nếu app hiển thị `dueDate` của KEV thì phải nói rõ ngữ nghĩa mới. Điểm quyết định
SSVC lấy từ `https://cveawg.mitre.org/api/cve/{CVE-ID}` (không cần auth), trong
`containers.adp` của CISA-ADP.

## 4. Bộ công cụ MCP đề xuất

Server: `secscan-mcp`. Tiền tố `sec_`. (Theo quy ước tại `CLAUDE.md`.)

**Tài sản & phạm vi**
`sec_asset_add` · `sec_asset_list` · `sec_asset_verify` · `sec_asset_remove`

**Quét**
`sec_scan_web` (L1/L2 một site) · `sec_scan_host` (L3 qua SSH) · `sec_scan_dns`
· `sec_scan_tls` · `sec_scan_ports` · `sec_scan_status` · `sec_scan_cancel`

**Kết quả**
`sec_findings` (lọc theo mức/tài sản/trạng thái) · `sec_finding_get`
· `sec_finding_ack` (chấp nhận rủi ro, kèm lý do + hạn) · `sec_finding_fixed`
· `sec_diff` (so hai lần quét — cái gì mới, cái gì đã vá)

**Tra cứu**
`sec_cve_lookup` (OSV+EPSS+KEV cho 1 CVE) · `sec_package_audit` (danh sách gói → CVE)

**Báo cáo**
`sec_report` (tổng hợp + điểm) · `sec_explain` (AI giải thích 1 phát hiện bằng
tiếng Việt: rủi ro thật là gì, khai thác được không, sửa thế nào)

**Định kỳ**
`sec_schedule_set` · `sec_schedule_list`

Nguyên tắc: mọi tool đều gọi cùng `api::*_value()` mà REST handler dùng — agent
và người không thể lệch nhau. Đây là quy ước bắt buộc trong repo.

## 5. Mô hình dữ liệu

```sql
-- Tài sản. KHÔNG quét được nếu verified_at IS NULL.
CREATE TABLE IF NOT EXISTS assets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,               -- 'website' | 'host' | 'domain'
  target TEXT NOT NULL UNIQUE,      -- 'https://x.vn' | 'ssh://user@1.2.3.4'
  label TEXT NOT NULL DEFAULT '',
  verify_method TEXT,               -- 'dns-txt' | 'well-known' | 'meta' | 'local' | 'manual'
  verify_token TEXT,
  verified_at INTEGER,              -- NULL = chưa xác minh → chặn quét
  ssh_ref TEXT,                     -- id connection bên ssh-manager, KHÔNG chứa credential
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS scans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  asset_id INTEGER NOT NULL,
  layer TEXT NOT NULL,              -- 'passive' | 'active-light' | 'host'
  status TEXT NOT NULL,             -- 'running' | 'done' | 'failed' | 'cancelled'
  score INTEGER, grade TEXT,        -- 0-100, A+..F
  started_at INTEGER NOT NULL, finished_at INTEGER,
  error TEXT, raw TEXT NOT NULL DEFAULT '{}'
);

-- fingerprint = khoá ổn định để so hai lần quét và để dedupe.
CREATE TABLE IF NOT EXISTS findings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  scan_id INTEGER NOT NULL, asset_id INTEGER NOT NULL,
  fingerprint TEXT NOT NULL,
  severity TEXT NOT NULL,           -- critical|high|medium|low|info
  category TEXT NOT NULL,           -- tls|headers|cookies|cors|dns|cve|ssh|exposure
  title TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
  evidence TEXT NOT NULL DEFAULT '{}',
  cve TEXT, epss REAL, kev INTEGER NOT NULL DEFAULT 0,
  remediation TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'open',   -- open|acked|fixed|regressed
  ack_reason TEXT, ack_until INTEGER,
  first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_find_fp ON findings(asset_id, fingerprint);
```

`fingerprint` là thứ làm cho `sec_diff` chạy được: cùng dấu vân tay qua hai lần
quét = cùng một vấn đề, nên biết được cái nào **mới**, cái nào **đã vá**, cái nào
**tái phát** (`regressed`) — thay vì mỗi lần quét lại đổ ra một đống phẳng.

## 6. Rào chắn quyền sở hữu — phần không được cắt bớt

Không có `verified_at` thì `sec_scan_*` **trả lỗi**, không phải cảnh báo.

Bốn cách xác minh — thứ tự đúng bằng độ mạnh:

| Cách | Làm gì | Độ mạnh |
|---|---|---|
| `dns-txt` | `senclaw-verify=<32 hex>` ở TXT **apex** | **Mạnh nhất** — đòi quyền sửa zone, tập chủ thể nhỏ và được kiểm soát chặt |
| `dns-cname` | `senclaw-<token>.example.com CNAME verify.senclaw.local` | Mạnh — **bắt buộc phải có** vì lý do kỹ thuật dưới đây |
| `well-known` | token tại `/.well-known/senclaw-verify` | Trung bình — chứng minh quyền web server, **không** phải quyền domain |
| `meta` | `<meta name="senclaw-verify" content="…">` trong `<head>` | Yếu nhất — chỉ cần chèn được HTML là giả được (CMS, stored XSS, template) |
| `local` | đích là RFC1918 / loopback, người dùng chủ động khai | dành cho hạ tầng nội bộ |

⚠️ **Vì sao `dns-cname` không phải tuỳ chọn mà là bắt buộc:** nhiều domain có apex bị
CNAME-flatten (Cloudflare, Netlify…), mà **bản ghi CNAME không thể đi kèm TXT** — giới
hạn của DNS. Chỉ làm mỗi `dns-txt` là một phần khách hàng sẽ không bao giờ xác minh được.
Snyk ghi rõ điều này trong tài liệu của họ và cung cấp CNAME làm đường lui.

**Token phải gắn với tenant, không được là chuỗi ngẫu nhiên trần.** Một
`senclaw-verify=<random>` thuần có thể bị chép lại: ai đọc được zone cũng dán được giá
trị đó sang tenant của mình. Đây chính là vấn đề mà bản nháp IETF `acme-dns-persist`
giải bằng tham số `accounturi` — nhúng định danh tenant vào giá trị và kiểm lại khi xác minh.

### 6.1 Xác minh một lần là chưa đủ

Hai thông lệ đối lập nhau, và cái đúng đã rõ:

- **Sai:** tài liệu cũ của Detectify từng bảo *"xác minh xong có thể gỡ bản ghi đi"*.
  Đó là xác-minh-một-lần-vĩnh-viễn: một subdomain sau này đổi chủ vẫn còn "đã xác minh".
- **Đúng:** tài liệu **hiện hành** của họ đã đảo ngược — *"Đừng gỡ bản ghi sau khi xác
  minh thành công. Nếu không tìm thấy bằng chứng lúc kiểm lại, tài sản có thể chuyển về
  chưa xác minh và **việc quét sẽ tạm dừng**."*

Vậy nên: **bắt bản ghi phải tồn tại lâu dài**, kiểm lại định kỳ, và **mất bản ghi =
thu hồi phạm vi ngay**. Về chu kỳ, CA/Browser Forum đã tính hộ: từ 15/3/2026 thời hạn
tái dùng kết quả kiểm soát tên miền tối đa là **200 ngày**. Đề xuất chặt hơn:
kiểm lại **mỗi 30 ngày**, **hết hạn cứng ở 90 ngày**, và **xác minh lại từ đầu khi DNS
đổi đáng kể** (đổi NS, chuyển nhà đăng ký, đổi A/AAAA sang ASN khác).

### 6.2 Cho phép L1 khi CHƯA xác minh — có cơ sở ngành

Đây là điều chỉnh so với bản nháp đầu của tôi, và nó theo đúng thông lệ tốt nhất đang có:

> **Snyk API & Web**: *"Trước khi xác minh xong, bạn chỉ chạy được **Lightning scan** —
> quét nông tập trung vào SSL/TLS, HTTP header và cookie."*
> **Pentest-Tools**: "Light scan" (thụ động) mở, "Deep scan" phải có tài khoản trả phí.

Tức là **quan sát thụ động được phép trước khi xác minh; chỉ việc gửi payload mới đòi
bằng chứng sở hữu.** Trùng khít với phân lớp L1/L2/L3 ở §3.1. Cho nên:

- **L1 (thụ động)** — cho chạy khi chưa xác minh. Nó chỉ làm đúng việc một trình duyệt
  làm khi ghé thăm, cộng vài truy vấn DNS.
- **L2 / L3** — **bắt buộc** `verified_at` còn hiệu lực.

Đáng biết để hiệu chỉnh kỳ vọng: **phần lớn scanner thương mại lớn không xác minh kỹ
thuật gì cả.** Tenable, Rapid7, Intruder chỉ dựa vào hợp đồng + cam kết + bồi hoàn.
Qualys thì chốt bằng **kho IP của tài khoản** — tên miền phân giải ra IP không nằm
trong kho thì **kết quả bị giữ lại, không trả về**. Chỉ Detectify, Snyk và
Acunetix/Invicti thực sự có xác minh DNS/file/meta.

Một điều khoản của Tenable đáng học vì tính răn đe: nếu bên thứ ba hỏi về một lần quét,
Tenable có quyền **cung cấp thông tin liên hệ của khách hàng cho chủ hệ thống bị quét
và cho cơ quan chức năng**.

Với `host`: có tài khoản SSH đăng nhập được **chính là** bằng chứng sở hữu.

Thêm ba chốt nữa:

- **Giới hạn tốc độ** mặc định ≤ 5 request/giây/tài sản, sửa được nhưng có trần cứng.
- **Nhật ký bất biến**: mọi lần quét ghi lại ai/khi nào/mục tiêu/lớp — để chứng minh phạm vi nếu bị hỏi.
- **Chế độ chỉ-đọc trên SSH**: tái dùng khuôn `CommandFilter` của ssh-manager, allowlist theo lệnh đầu (`cat`, `ss`, `dpkg-query`, `sshd -T`, …). Không có shell tự do.

### 6.3 Đừng để chính scanner thành công cụ tấn công

Một app nhận URL từ người dùng rồi tự đi gọi chính là định nghĩa của SSRF. Bốn chốt:

- **Chặn ở tầng phân giải tên** theo **toàn bộ registry special-purpose của IANA**,
  đừng tự bốc vài dải: `0.0.0.0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16`,
  `172.16/12`, `192.0.0/24`, `192.0.2/24`, `192.88.99/24`, `192.168/16`, `198.18/15`,
  `198.51.100/24`, `203.0.113/24`, `224/4`, `240/4` — và IPv6: `::/128`, `::1/128`,
  `::ffff:0:0/96`, `fc00::/7`, `fe80::/10`, `2001:db8::/32`, `2002::/16`.
  Resolve trước rồi mới kiểm — và **kiểm lại sau MỖI lần redirect**.

  Điểm cuối metadata, kèm ba cái bẫy:

  | Cloud | Địa chỉ | Bẫy |
  |---|---|---|
  | AWS IMDS | `169.254.169.254`, **`fd00:ec2::254`** | IPv6 là ULA, **không** nằm trong `fe80::/10` |
  | GCP | `169.254.169.254`, **`fd20:ce::254`** | tiền tố ULA khác AWS |
  | **Azure (platform)** | **`168.63.129.16`** | **trông y hệt IP công cộng — lọt qua mọi bộ lọc dải-riêng ngây thơ** |
  | Alibaba ECS | `100.100.100.200` | nằm trong CGNAT `100.64/10` |

  Chặn **cả `169.254.0.0/16`**, không chỉ `.254` — và nhớ địa chỉ đó có vô số cách viết
  tương đương: `2852039166`, `0251.0376.0251.0376`, `::ffff:169.254.169.254`,
  `169.254.169.254.nip.io`.

  Vì app này **chỉ quét hạ tầng của mình**, cách đúng nhất là **danh sách cho phép**
  chứ không phải danh sách chặn: chỉ đi tới IP nằm trong một scope grant đã xác minh.
  Deny-list luôn có đường vòng; allow-list thì không.
- **Chống DNS rebinding**: ghim IP đã resolve cho suốt vòng đời request, đừng để
  tầng mạng resolve lại giữa lúc kiểm và lúc kết nối.
- **Không bao giờ quét theo IP trần trên hosting dùng chung** — một IP có thể chứa
  hàng nghìn vhost không liên quan. Xác minh và quét theo **tên miền**, gửi `Host`/SNI rõ ràng.
- **Tự khai danh trong `User-Agent`** kèm URL liên hệ. Nó biến "kẻ lạ đang dò" thành
  "một công cụ có người chịu trách nhiệm, gửi mail được".

Ngoại lệ có chủ ý: mục `local` ở bảng trên cho phép RFC1918 — nhưng đó là khi người
dùng **chủ động khai** hạ tầng nội bộ của mình, không phải khi một URL bất kỳ resolve
về đó.

## 7. Vai trò của AI — và chỗ AI không được đụng vào

AI **không** đi tìm lỗ hổng. Việc tìm là của các đầu dò tất định, có thể tái lập.
AI làm ba việc mà bảng biểu làm dở:

1. **Phân loại & lọc nhiễu.** "Thiếu CSP" trên trang tĩnh không có form khác hẳn
   trên trang có đăng nhập. AI đọc bối cảnh rồi hạ/nâng mức, kèm lý do.
2. **Giải thích bằng tiếng Việt.** Từ `DMARC p=none` thành "ai cũng giả mạo được
   email @tencongty.vn mà không bị chặn; đổi sang `p=quarantine` sau khi xem báo cáo 2 tuần".
3. **Xếp thứ tự việc cần làm.** 200 phát hiện → "tuần này sửa 3 cái này trước, vì…".

Ràng buộc kỹ thuật đã biết của bridge `llm.request` (xem [[space-app-llm-bridge-output-ceiling]]):
`maxTokens` đặt **32000**, chia nhỏ đầu vào ~2000 ký tự/lần, và **không tin
`finish`** — phát hiện cắt cụt bằng hình dạng câu trả lời. Tiếng Việt tốn token
hơn nhiều so với số ký tự nhìn thấy.

Với điều tra sâu, dùng `agent.run` (bridge action này **có** chạy, khác với
`mcp.call` đang là stub) kèm **allowlist tool theo từng lần gọi** — cho agent đúng
bộ công cụ chỉ-đọc, không hơn.

## 8. Chấm điểm

### 8.1 Khuôn header — theo MDN HTTP Observatory

⚠️ **Có hai codebase khác nhau.** Dùng bản **MDN** (`github.com/mdn/mdn-http-observatory`,
bảng hệ số ở `src/grader/charts.js`), **không** dùng `mozilla/http-observatory` đã nghỉ —
giá trị hai bên lệch nhau.

Cơ chế: khởi điểm **100**, sàn 0, trần **145**. **Hai vòng: trừ trước, cộng sau —
và chỉ cộng nếu điểm sau khi trừ ≥ 90** (`MINIMUM_SCORE_FOR_EXTRA_CREDIT = 90`).

| Điểm | Hạng | | Điểm | Hạng |
|---|---|---|---|---|
| 100+ | **A+** | | 50–59 | C |
| 90–99 | A | | 45–49 | C− |
| 85–89 | A− | | 40–44 | D+ |
| 80–84 | B+ | | 30–39 | D |
| 70–79 | B | | 25–29 | D− |
| 65–69 | B− | | 0–24 | **F** |
| 60–64 | C+ | | | |

Hệ số đáng nhớ (trích): CSP `default-src 'none'` **+10** · `unsafe-inline` **−20** ·
không có CSP **−25**. Cookie Secure+HttpOnly+SameSite **+5** · cookie phiên thiếu
HttpOnly **−30** · **cookie phiên thiếu Secure −40**. **CORS `*` kèm credentials −50**.
HSTS preload **+5** · không có **−20**. SRI không có mà script ngoài không an toàn **−50**.
Referrer-Policy chặt **+5**. **X-Frame-Options `DENY`/`SAMEORIGIN` +5** (bản cũ là 0).
**CORP `same-origin`/`same-site` +10** — phép kiểm mới, bản cũ không có.
**`X-XSS-Protection` đã bỏ hẳn** — rubric 2026 không được có mục này.

### 8.2 Khuôn TLS — theo SSL Labs

Trọng số: chứng thư là **cổng đạt/trượt** (không tính điểm) · Protocol Support **30%**
· Key Exchange **30%** · Cipher Strength **40%**. Tổng → chữ: ≥80 A · ≥65 B · ≥50 C ·
≥35 D · ≥20 E · <20 F.

Nhưng **trong thực tế các mức trần mới là thứ quyết định**, không phải phép tính:

- **F tự động**: sai tên miền · hết hạn · tự ký · CA không tin cậy · bị thu hồi ·
  ký MD5 · **SSL 3.0 là giao thức tốt nhất** · cipher export · **Heartbleed, DROWN, ROBOT, Ticketbleed**
- **Trần B**: khoá < 2048 · **còn nhận TLS 1.0 hoặc 1.1** · **còn RC4** · chuỗi thiếu ·
  **không có forward secrecy** · không có AEAD
- **Trần C**: CRIME · 3DES với TLS 1.1+ · không có TLS 1.2
- **A−**: không có TLS 1.3 · thiếu HSTS
- **A+ cần**: không cảnh báo nào + **HSTS max-age ≥ 6 tháng (15 552 000 s)** + TLS 1.3
  + không cert SHA-1 + `TLS_FALLBACK_SCSV`

→ Cài đặt theo **luật trần** trước, phép cộng trừ sau. Ngược lại sẽ ra hạng A cho
một site còn mở TLS 1.0.

### 8.3 Xếp ưu tiên vá — dùng EPSS ngưỡng 0.1

Số liệu hiệu quả của FIRST, đáng để chọn ngưỡng thay vì đoán:

| Chiến lược | Công sức | Hiệu quả | Độ phủ |
|---|---|---|---|
| Vá mọi thứ CVSS ≥ 7 | **50.7%** | ~6% | 74.6% |
| **EPSS ≥ 0.1** | **2.7%** | **45.5%** | 63.2% |

Tức là lọc theo CVSS ≥ 7 bắt anh vá **một nửa** kho lỗ hổng để bắt được 6% cái
thật sự bị khai thác. EPSS ≥ 0.1 chỉ tốn 2.7% công sức. Chỉ có 1.5–3% lỗ hổng công
bố từng bị khai thác thật, nên phân phối cực lệch đuôi.

⚠️ FIRST cảnh báo thẳng: lấy ngưỡng "90–100%" chỉ chọn được **top 0.2%** — quá hẹp.

**KEV là phép đè cứng, không phải trọng số**: một mục KEV điểm CVSS 5.3 phải xếp
trên một mục không-KEV điểm 9.8. Trường `knownRansomwareCampaignUse` là cờ leo
thang hữu ích nhất trong KEV.

### 8.4 Ba nguyên tắc

- **Không lấy trung bình.** Một lỗi CRITICAL kéo hạng xuống F, không được các mục
  màu xanh khác pha loãng. (Chính là cơ chế "trần" của SSL Labs.)
- **Điểm là để so với chính mình theo thời gian**, không phải để khoe. Màn hình
  chính hiển thị *xu hướng* và *diff*, không phải một con số to.
- **CVSS: phải đỡ cả v2/v3.0/v3.1/v4.0 cùng lúc, và coi "thiếu điểm v4" là chuyện
  bình thường** — ~74% CVE 2025 không có điểm v4. Lưu ý **v4.0 không có công thức
  đóng**: nó tra bảng MacroVector rồi nội suy, nên **không tự cài lại được như v3.1** —
  phải dùng bảng của FIRST hoặc `FIRSTdotorg/cvss-v4-calculator`.

## 9. Lộ trình

| Giai đoạn | Nội dung | Vì sao trước/sau |
|---|---|---|
| **P1** | scope.rs + xác minh sở hữu + khung DB/MCP/UI | Rào chắn phải có **trước** đầu dò đầu tiên |
| **P2** | L1 thụ động: header, cookie, TLS, DNS | Giá trị/công sức cao nhất; đã chứng minh ra lỗi thật |
| **P3** | vuln.rs: OSV + KEV + EPSS | Keyless, nhanh, dùng lại cho cả web lẫn host |
| **P4** | L3 host qua SSH (chỉ đọc) + audit gói | Cần P3 xong mới có chỗ đổ dữ liệu |
| **P5** | L2 chủ động nhẹ + quét cổng | Rủi ro cao nhất → làm sau cùng khi rào chắn đã chắc |
| **P6** | AI phân loại/giải thích + báo cáo + định kỳ + cảnh báo | Cần dữ liệu thật để chỉnh prompt |

Đề xuất bắt đầu ở **P1+P2**: đủ để vá ba lỗi đã tìm thấy ở §1.

## 10. Bẫy đã biết (đo được, không phải phỏng đoán)

1. **Dò TLS bằng client hiện đại cho ÂM TÍNH GIẢ.** `openssl s_client -tls1` trả
   `no protocols available` — đó là *client* từ chối, chưa gửi gói nào. Thêm
   `-cipher 'DEFAULT@SECLEVEL=0'` thì example.com **có** nhận TLS 1.0. Scanner ngây
   thơ sẽ báo "an toàn" khi thực ra không.
   **Và SECLEVEL=0 cũng chỉ là chỗ dựa tạm** — xem §12.2, cách bền vững là tự gửi
   ClientHello thô.
2. **Dùng `HEAD` để lấy header cho DƯƠNG TÍNH GIẢ hàng loạt.** `vnexpress.net`
   trả `406` cho HEAD nhưng `200` cho GET → scanner sẽ báo thiếu *mọi* header.
   Bắt buộc GET, chặn body sớm.
3. **404 mềm.** Nhiều site trả 200/406 cho đường dẫn không tồn tại. Phải lấy mốc
   bằng 2 đường dẫn ngẫu nhiên trước khi kết luận "tệp này tồn tại".
4. **OSV Maven cần `groupId:artifactId`.** Sai định danh trả mảng rỗng, **không**
   báo lỗi — im lặng bảo "an toàn".
5. **Kernel `linux` trả 1000 CVE** (chạm trần phân trang), gần hết không áp dụng
   được. Phải đặc cách, lọc qua KEV/EPSS.
6. **Không hardcode URL app khác.** ssh-manager chạy cổng ngẫu nhiên (đo được:
   57426 và 64863, hai tiến trình). Phải hỏi `/api/space/apps` lúc chạy.
7. **`mcp.call` của bridge là stub.** Muốn gọi app khác thì POST thẳng JSON-RPC
   tới `/api/mcp/message` của app đó (khuôn autotest → mini-browser).
8. **crt.sh hay 502** (gặp ngay lúc thử). Cần đường lui nếu dùng Certificate Transparency.
9. **Đừng lặp lại keychain của ssh-manager** — nó lưu mật khẩu/khoá riêng dạng
   **plaintext JSON**. App bảo mật mà làm vậy thì tự mâu thuẫn.
10. **DKIM không enumerate được** — selector phải biết trước. Hiển thị "không kiểm
    tra được" chứ đừng báo "không có DKIM".

## 11. Danh mục phép kiểm cụ thể

Đây là phần để lập trình thẳng thành hàm, không phải mô tả chung chung.

### 11.0 Đối chiếu OWASP Top 10:2025 — và thành thật về giới hạn

**Bản 2025 là bản hiện hành** (công bố 11/2025). Hai hạng mục mới: **A03 Software
Supply Chain Failures** và **A10 Mishandling of Exceptional Conditions**. **SSRF bị bỏ
khỏi danh sách riêng**, gộp vào A01. Và đáng chú ý nhất với app này:
**A02 Security Misconfiguration nhảy từ #5 lên #2** — đúng thứ scanner làm tốt nhất.

| Hạng mục | Scanner tự dò được? |
|---|---|
| A02 Security Misconfiguration | **Tốt** — vùng sở trường |
| A04 Cryptographic Failures | **Một phần** — tư thế TLS được; mã hoá lúc lưu thì không |
| A05 Injection | **Tốt, nhưng cần dò chủ động** — ngoài phạm vi app này |
| A10 Mishandling of Exceptional Conditions | **Một phần** — lộ stack trace được; logic fail-open thì không |
| A01 Broken Access Control | **Một phần** — endpoint thiếu auth được; phân quyền theo đối tượng/vai trò **không** |
| A07 Authentication Failures | **Một phần** — cookie/phiên/khoá tài khoản được; thiết kế MFA thì không |
| A03 Supply Chain · A06 Insecure Design · A09 Logging Failures | **Không** |

Câu trích đáng dán lên UI, từ chính tài liệu OWASP ZAP:

> *"logical vulnerabilities, such as broken access control, will not be found by any
> active or automated vulnerability scanning"*

→ Báo cáo **phải nói rõ mình không phủ cái gì**. Một scanner ngụ ý "sạch = an toàn"
còn nguy hiểm hơn không có scanner. A01 và A06 cần con người đọc.

Dùng mã test của **OWASP WSTG** (`WSTG-CONF-*`, `WSTG-CRYP-*`, `WSTG-INFO-*`, …) làm
phân loại phát hiện — đó là ngôn ngữ chung của ngành cho câu hỏi "phép kiểm nào sinh ra
phát hiện này".

### L1 — Thụ động (1 GET + DNS + 1 bắt tay TLS)

**Security header.** Mức độ dưới đây **không phải tự đặt** — chúng theo hành vi
trình duyệt thật năm 2026. Đặt sai mức là cách nhanh nhất để scanner mất uy tín.

| Header | Kiểm gì | Mức nếu thiếu | Vì sao mức đó |
|---|---|---|---|
| `Strict-Transport-Security` | `max-age` ≥ 15768000 (6 tháng) | MEDIUM | `max-age=0` phải là **HIGH** — nó *tắt* HSTS |
| `Content-Security-Policy` | xem quy tắc chống dương-tính-giả bên dưới | MEDIUM | **78% web không có CSP** — gọi đây là "critical" là phóng đại |
| `X-Content-Type-Options` | `= nosniff` | LOW | không có mặt trái, nhưng tác động thấp |
| `X-Frame-Options` **hoặc** CSP `frame-ancestors` | một trong hai là đủ | MEDIUM | **Kiểm "có chống đóng khung không", đừng kiểm riêng XFO** — nếu có `frame-ancestors` thì trình duyệt **bỏ qua XFO** hoàn toàn |
| `Referrer-Policy` | giá trị có **nguy hiểm** không | **INFO** nếu thiếu | Mặc định của trình duyệt đã là `strict-origin-when-cross-origin` (an toàn). Thiếu ≠ lỗi. **Phát hiện thật là giá trị xấu**: `unsafe-url`, `origin`, `no-referrer-when-downgrade` → LOW/MEDIUM |
| `Permissions-Policy` | giá trị `*` nguy hiểm | **INFO** | **Firefox và Safari KHÔNG hỗ trợ** (mọi phiên bản). Chấm nặng một header chỉ chạy trên Chromium là không trung thực |
| `Cross-Origin-Resource-Policy` | `same-origin`/`same-site` | INFO | Observatory chỉ chấm CORP, **không chấm COOP/COEP** |
| `Cross-Origin-Opener-Policy` / `-Embedder-Policy` | có mặt | INFO | `COEP: require-corp` **làm vỡ mọi nhúng bên thứ ba** thiếu CORP → khuyên bật đại trà là có hại |
| `X-XSS-Protection` | **có mặt = nên bỏ đi** | — | Đã phế; MDN cảnh báo nó *tạo ra* lỗ XSS. Observatory đã **xoá hẳn phép kiểm này** |

**Bốn quy tắc CSP để không báo sai** — đây là chỗ scanner hay sai nhất:

1. **Có nonce hoặc hash trong `script-src` → trình duyệt BỎ QUA `'unsafe-inline'`.**
   Vậy thì **không được báo lỗi**. Báo là dương tính giả.
2. **`'strict-dynamic'` mà KHÔNG có nonce/hash** = chính sách hỏng (chặn hết script),
   không phải chính sách chặt → đây mới là lỗi (Observatory tính `-25`, "header invalid").
3. Thiếu `base-uri` là lỗi **HIGH** thật — chèn thẻ `<base>` đổi hướng mọi script tương đối.
4. `'unsafe-inline'` chỉ ở `style-src` → **LOW**, không phải HIGH.

Số liệu để hiệu chỉnh kỳ vọng (Web Almanac 2025): chỉ **21.9%** trang có CSP, và
trong số đó **92% vẫn chứa `'unsafe-inline'`**, 77% chứa `'unsafe-eval'`. Chính sách
phổ biến nhất là `upgrade-insecure-requests` đứng một mình.

**Một phép kiểm gần như không ai làm:** XFO nhiều giá trị. Theo thuật toán của WHATWG,
nhiều header XFO mà **tất cả đều không hợp lệ** thì bị coi như **không có** — tức là
**hỏng theo hướng mở**. Cấu hình sai kiểu này mất sạch chống đóng khung mà không ai biết. → MEDIUM.

**Lộ thông tin** — `Server`, `X-Powered-By`, `X-AspNet-Version`, `X-Generator`
chứa số phiên bản → LOW, nhưng **ghép với OSV để thành CVE cụ thể** thì có thể lên HIGH.
(Đo thật: `server: vne-qt-fe-bot-1` lộ tên host nội bộ.)

OWASP có sẵn danh sách **88 header nên gỡ** dạng máy đọc được — dùng luôn:
`raw.githubusercontent.com/OWASP/www-project-secure-headers/master/ci/headers_remove.json`
(⚠️ nhánh là `master`, không phải `main`; URL trên `owasp.org` thì 404). Nó phủ cả
những thứ ít ai nghĩ tới: `SourceMap`/`X-SourceMap` (lộ mã nguồn), `X-B3-*`/`X-Datadog-*`
(tracing), `X-Envoy-*`/`X-Kong-*`/`X-Kubernetes-PF-*` (hạ tầng), `X-Nextjs-*`.
Trường `last_update_utc` dùng làm mốc phát hiện thay đổi.

> **Chỗ để khác biệt:** securityheaders.com chỉ *báo* các header lộ thông tin mà
> **không trừ điểm**; Observatory thì bỏ qua hẳn. Chấm điểm mục này là điểm cộng thật.

**Cookie** — với mỗi `Set-Cookie`: thiếu `Secure` (MEDIUM trên HTTPS; **cookie phiên
thiếu `Secure` là nặng nhất** — Observatory trừ 40), thiếu `HttpOnly` (MEDIUM nếu là
cookie phiên), `SameSite=None` mà không `Secure` (HIGH), `Domain` quá rộng (LOW).

⚠️ **Không có `SameSite` là phát hiện THẬT, không phải hình thức.** Câu "trình duyệt
hiện đại mặc định Lax rồi" là **sai**: chỉ Chrome/Edge mặc định `Lax`. **Firefox coi
như `None`** — bug 1617609 đã đóng **WONTFIX** sau khi thử ở bản 96 rồi phải lùi lại.
Safari không hỗ trợ. → LOW, lên MEDIUM với cookie phiên.

Đáng kiểm thêm: cookie tên `__Host-…`/`__Secure-…` mà **vi phạm ràng buộc của tiền tố**
(thiếu `Secure`, có `Domain`, `Path` khác `/`) → trình duyệt **âm thầm từ chối**, vừa
là lỗi chức năng vừa là lỗi bảo mật.

**TLS** — hết hạn < 14 ngày (HIGH) / < 30 ngày (MEDIUM); tên không khớp (HIGH);
chuỗi tự ký hoặc đứt (HIGH); **chấp nhận TLS 1.0/1.1 (MEDIUM)** — nhớ bẫy
SECLEVEL ở §10.1; khoá RSA < 2048 (HIGH); chữ ký SHA-1 (HIGH).

**DNS/email** — `SPF` thiếu (MEDIUM) hoặc kết thúc `~all`/`?all` thay vì `-all` (LOW);
`DMARC` thiếu (MEDIUM) hoặc `p=none` (MEDIUM — chỉ giám sát, **không chặn giả mạo**);
`CAA` thiếu (LOW); `DNSSEC` (bản ghi `DS`) thiếu (LOW).
DKIM: chỉ kiểm được khi biết selector → báo "không kiểm tra được", đừng báo "không có".

### L2 — Chủ động nhẹ (vài chục request hợp lệ, có giới hạn tốc độ)

**Đường dẫn lộ** — GET và so với mốc 404 mềm (§10.3):
`/.git/HEAD` · `/.env` · `/.DS_Store` · `/backup.sql` · `/.svn/entries`
· `/config.php.bak` · `/phpinfo.php` · `/server-status` · `/actuator/health`
· `/.well-known/security.txt` (thiếu = INFO, không phải lỗi)
· `robots.txt` + `sitemap.xml` (đọc để mở rộng bề mặt, không phải phát hiện).
Directory listing: phản hồi chứa `Index of /` → MEDIUM.

**CORS sai** — phân biệt cho đúng, vì hai thứ này khác nhau về mức độ:

| Trường hợp | Mức | Lý do |
|---|---|---|
| ACAO **phản chiếu lại đúng `Origin` mình gửi** + `Allow-Credentials: true` | **HIGH** | Đây mới là lỗ thật. Observatory trừ **−50**, hình phạt nặng nhất trong toàn hệ thống của họ |
| Chấp nhận `Origin: null` + credentials | **HIGH** | Khai thác được từ iframe `sandbox` bất kỳ |
| `ACAO: *` **kèm** `Allow-Credentials: true` | **LOW** | **Trình duyệt chặn tổ hợp này** — nó hỏng theo hướng đóng. Báo CRITICAL là dương tính giả kinh điển. Nhưng vẫn đáng nêu: nó chứng tỏ lập trình viên *định* cho phép credentials, và cách "sửa" thường gặp là chuyển sang phản chiếu `Origin` — tức là tạo ra đúng lỗi HIGH ở trên |
| `ACAO: *` đơn thuần trên endpoint dữ liệu riêng | MEDIUM | Không có credentials thì trình duyệt cho qua, nhưng vẫn phơi body cho mọi origin — nguy khi phân quyền dựa vào vị trí mạng (intranet/VPN/IP allowlist). Client không phải trình duyệt thì bỏ qua CORS hoàn toàn |
| ACAO động mà **thiếu `Vary: Origin`** | LOW→MEDIUM | Cache dùng chung có thể trả ACAO của origin A cho origin B. Lên MEDIUM khi thấy dấu hiệu cache (`Age`, `X-Cache`, header CDN) |

**Cách dò:** gửi `Origin: https://evil.example`, rồi gửi **origin thứ hai khác hẳn** —
hai lần cùng được phản chiếu mới chắc là reflection chứ không phải trùng allowlist.
Kiểm **preflight `OPTIONS` và request thật riêng biệt** — server phản chiếu ở một
trong hai vẫn là dính. Thử thêm các kiểu khớp hớ: `https://target.com.evil.example`
(khớp tiền tố), `https://eviltarget.com` (khớp hậu tố), `https://evil.example?target.com`
(regex không neo).

> Trong lời khuyên khắc phục **phải nói rõ: đừng sửa bằng cách phản chiếu `Origin`.**
> Đó là cách "sửa" phổ biến nhất và nó biến lỗi LOW thành lỗi HIGH.

(Đo thật trên daemon máy này: `ACAO: *` toàn bộ `/api/*`, không auth — rơi vào hàng
MEDIUM ở trên, và vì daemon không có lớp xác thực nào nên đáng quan tâm hơn mức đó.)

**Open redirect** — `?next=`, `?url=`, `?redirect=`, `?return=` trỏ tới host lạ;
theo dõi `Location` mà **không** đi theo. Chuyển hướng ra ngoài → MEDIUM.

**Mixed content** — trang HTTPS nhúng `http://` cho script/css/iframe → MEDIUM.

**Thư viện JS cũ** — rút tên+phiên bản từ đường dẫn/comment/biến toàn cục, tra OSV `npm`.

### L3 — Máy chủ qua SSH (chỉ đọc, allowlist lệnh)

| Kiểm | Lệnh (chỉ đọc) | Cờ đỏ |
|---|---|---|
| Cấu hình SSH | `sshd -T` (**không đọc file thô** — xem dưới) | `permitrootlogin yes`, `passwordauthentication yes`, KEX/cipher yếu |
| Cổng đang nghe | `ss -tlnp` | dịch vụ bind `0.0.0.0` mà lẽ ra chỉ nội bộ |
| Gói đã cài | `dpkg-query -W -f='${Package} ${Version}\n'` / `rpm -qa` | → đẩy sang OSV (§3.2) |
| Nhân | `uname -r` | đặc cách, xem §10.5 |
| Tường lửa | `ufw status` / `firewall-cmd --list-all` / `nft list ruleset` | tắt hẳn |
| Tự động vá | `systemctl is-enabled unattended-upgrades` | không bật |
| Sudo | `cat /etc/sudoers.d/*` | `NOPASSWD: ALL` |
| Quyền tệp | `find / -perm -4000 -type f` (giới hạn độ sâu) | SUID lạ |
| Tài khoản | `awk -F: '$3>=1000' /etc/passwd`, `lastlog` | tài khoản không dùng, UID 0 trùng |

Bind `0.0.0.0` là mục đáng giá nhất — đó chính là lỗi tìm thấy trên máy này (§1).

**Ba điểm dễ sai ở tầng này:**

- **Phải chạy `sshd -T`, không được parse `/etc/ssh/sshd_config` thô.** Dòng
  `Include /etc/ssh/sshd_config.d/*.conf` giờ là chuẩn và thường nằm **đầu file**, mà
  OpenSSH lại **lấy giá trị khớp đầu tiên** — nên file trong `sshd_config.d/` đè lên
  file chính. Đọc file thô sẽ ra kết luận ngược.
- **`PasswordAuthentication` mặc định là `yes`.** Không thấy dòng nào trong config
  **không có nghĩa là đã tắt**. Tương tự `X11Forwarding`: upstream mặc định `no` nhưng
  **nhiều bản phân phối phát hành với `yes`**.
- **`Protocol 2` là rác cấu hình, không phải biện pháp.** SSHv1 đã bị bỏ từ OpenSSH 7.4
  (2017). Thấy dòng đó thì báo "thừa", đừng báo "đạt".

Kiểm theo phiên bản cũng đáng giá: OpenSSH 9.6 vá **Terrapin (CVE-2023-48795)**,
9.8 vá **regreSSHion (CVE-2024-6387** — RCE tiền xác thực, dính 8.5p1–9.7p1).

Vài phép kiểm host cho tín hiệu cao mà hay bị bỏ sót:

- **`systemd-analyze security`** — chấm điểm phơi nhiễm 0–10 cho từng unit và **chỉ
  thẳng directive còn thiếu**. Đối chiếu tiến trình chạy bằng root với `ss -tlnp`:
  **root + nghe trên `0.0.0.0`** mới là trường hợp leo thang thật.
- **`auditd` chạy nhưng không có luật nào** (`auditctl -l` rỗng) — rất phổ biến và
  tương đương với không cài.
- **fail2ban chạy nhưng mọi jail đều tắt** — khác với "chưa cài", và nguy hiểm hơn vì
  tạo cảm giác đã có bảo vệ.
- **Tự động vá bật timer nhưng `apply_updates = no`** (RHEL) — tải mãi mà không bao giờ
  cài. Phải đọc **giá trị cấu hình**, không chỉ trạng thái unit.
- **Nhân đã vá trên đĩa nhưng chưa boot** — so `uname -r` với gói kernel mới nhất đã cài;
  `/var/run/reboot-required`, `needs-restarting -r`, `needrestart -b`.
- **SELinux/AppArmor**: tách hai lỗi khác nhau — (a) đang chạy ở chế độ permissive,
  (b) file cấu hình ghi disabled nên sau reboot sẽ không quay lại.
- `find` phải có **`-xdev`**, nếu không sẽ quét cả `/proc`, `/sys` và mọi mount mạng.

## 12. Chọn thư viện Rust

### 12.1 Bảng khuyến nghị

| Việc | Cách | Thư viện |
|---|---|---|
| Quét cổng TCP connect (không cần root) | **Rust thuần** | `tokio` + `socket2` + `rlimit` — tự viết ~200 dòng, không crate nào đáng phụ thuộc |
| Quét SYN / gói thô | **Bỏ, hoặc gọi nmap ngoài** | `pnet` không ra bản mới từ 2024-05-30; mà SYN cần root — trên hạ tầng của mình thì connect-scan là đủ |
| Đọc chứng thư (hạn, SAN, khoá, thuật toán ký) | **Rust thuần** | `x509-parser` 0.18.1 — nơi sinh ra gần hết phát hiện về cert |
| Kiểm chuỗi tin cậy + CRL | **Rust thuần** | `rustls-webpki` 0.103.13 (**không** kiểm OCSP) |
| Bắt tay TLS 1.2/1.3, lấy suite đã thoả thuận | **Rust thuần** | `rustls` + `tokio-rustls` |
| **Dò giao thức cũ + cipher yếu** | **Tự gửi ClientHello thô** | `tokio` + `tls-parser` 0.12.2 — xem §12.2 |
| Vân tay TLS phía server | **Rust thuần** | `rust_jarm` 0.3.10 (**JARM**, không phải JA4S — xem §12.4) |
| DNS | **Rust thuần** | `hickory-resolver` 0.26.1 |
| HTTP | **Rust thuần** | `reqwest` (workspace đang 0.12.28) — **nhớ bật HTTP/2** cho OSV |
| Phân tích HTML | **Rust thuần** | `scraper` 0.27 |
| **Phân tích CSP** | **Rust thuần** | **`content-security-policy` 0.8.1** — xem ghi chú dưới |
| Permissions-Policy và các header cấu trúc mới | **Rust thuần** | `sfv` 0.15 (header này được định nghĩa là `sf-dictionary`) |
| Đọc `Set-Cookie` | **Rust thuần** | crate `cookie` — **đừng dùng `headers::SetCookie`**, nó là `Vec<HeaderValue>` trần, không parse thuộc tính |
| HSTS preload | **Tự nhúng danh sách** | Không có crate nào nhúng/tra danh sách preload của Chromium |
| Regex | **Rust thuần** | `regex` 1.13 — xem §12.3 |
| Giới hạn tốc độ | **Rust thuần** | `governor` 0.10.4 (backoff thích ứng vẫn phải tự viết) |
| Hash favicon | **Rust thuần** | `murmur3` 0.5.2 |
| Đối chiếu CVE | **Rust thuần** | `osv` 0.3.0 (crate duy nhất; **không có** `osv-schema`), `cvss` 2.2.0 |
| Kiểm sâu ứng dụng web | **Gọi ngoài, tuỳ chọn** | `nuclei` — engine và templates **đều MIT** |

`Cargo.lock` đã sẵn `rustls`, `tokio-rustls`, `rustls-webpki`, `native-tls`,
`openssl`, `socket2`, `reqwest`, `regex`, `fancy-regex`. Cần thêm: `x509-parser`,
`tls-parser`, `hickory-resolver`, `scraper`, `governor`, `osv`, `cvss`, `murmur3`, `rust_jarm`.

**Chỗ khác biệt ít ai khai thác:** crate `content-security-policy` 0.8.1 chính là
**bản cài CSP của Servo** — nó cài đúng thuật toán W3C chứ không phải bộ dựng chuỗi,
với các hàm như `should_request_be_blocked()`. Nghĩa là app **mô phỏng được** "URL này
/ script inline này có bị chặn thật không", thay vì so khớp chuỗi directive. Toàn bộ
9 crate phụ thuộc vào nó đều là của Servo — **chưa scanner nào dùng**. Đổi lại: 18 bản
phát hành thì 8 bản phá vỡ tương thích, tài liệu mỏng.

Bổ sung nên lấy: **`google/csp-evaluator`** (Apache-2.0) có sẵn tập dữ liệu
`allowlist_bypasses/` — danh sách endpoint JSONP, domain host Angular, đường vòng Flash.
Đây là thứ rất khó tự dựng lại và tái dùng được ngay; thang mức độ của họ
(`HIGH=10 · SYNTAX=20 · MEDIUM=30 · STRICT_CSP=45 · INFO=60`) cũng chi tiết hơn hẳn
một phán quyết `unsafe-inline` đơn lẻ của Observatory.

### 12.2 Vì sao phải tự gửi ClientHello

Đây là điểm tôi **nhận định sai lúc đầu** và đã được sửa:

- `rustls` chỉ có `TLS12` và `TLS13`, không cờ nào bật được TLS 1.0/1.1. Nó cũng
  chỉ phơi ra **9 bộ cipher AEAD** — không có CBC, RC4, 3DES, static-RSA, EXPORT.
  Tức là **không dò được cipher yếu**, chứ không riêng gì giao thức cũ.
- Crate `openssl` *có* `SslVersion::{SSL3, TLS1, TLS1_1}` và `set_security_level`,
  nhưng nó chỉ là **binding** — khả năng thật nằm ở libssl được liên kết. **OpenSSL
  3.5 đã bỏ TLS 1.0/1.1 ra khỏi bản build mặc định**, SECLEVEL=0 không kéo lại được;
  OpenSSL 4.0.1 ra tháng 6/2026. Nghĩa là cùng một đoạn code sẽ chạy đúng trên máy
  libssl cũ và **âm thầm báo "không hỗ trợ TLS 1.0" trên máy mới** — đúng kiểu âm
  tính giả tệ nhất. (Trên máy này OpenSSL 3.6.3 homebrew vẫn còn TLS 1.0 nên thử ra
  kết quả đúng — nhưng đó là may, không phải bảo đảm.)

**Cách bền vững:** tự dựng ClientHello và đọc byte trả về — đúng cách `testssl.sh`
và `sslscan` làm. Không đụng tới mã hoá nên **không thư viện nào phủ quyết được**:

- Dò giao thức: bản ghi TLS `0x16` + version `0x0300`/`0x0301`/`0x0302`/`0x0303`.
  Có ServerHello = hỗ trợ; Alert `70` (protocol_version) hoặc RST = không.
- Dò cipher (≤TLS 1.2): chào đúng **một** suite mỗi lần; ServerHello vọng lại suite
  đó, hoặc Alert `40` (handshake_failure).
- Parse bằng `tls-parser` 0.12.2 — nó có sẵn bảng `TlsCipherSuite { enc, enc_size,
  mac, kx, … }` tra theo id, nên **bộ phân loại cipher yếu có sẵn**: `enc_size < 128`,
  `enc == Rc4|Null`, `mac == Md5`, `kx == Rsa` (không PFS).

### 12.3 Regex: chọn `regex`, không chọn engine backtracking

Đo trên toàn bộ tập vân tay công nghệ (7 992 mẫu): `regex` 1.13 biên dịch được
**99.75%**; 20 mẫu hỏng **đều** do look-around, **không mẫu nào dùng backreference**.

Dù `fancy-regex` biên dịch được cả 20, vẫn nên giữ `regex` làm engine chính:
đầu vào là **HTML của trang đang quét — do đối tượng kiểm soát**, và Wappalyzer đã
từng dính ReDoS thật hai lần (SNYK-JS-WAPPALYZER-597530 và -572854: một trang dựng
sẵn làm treo CLI). Bảo đảm thời gian tuyến tính của `regex` khiến cả lớp lỗi đó
**không tồn tại được**. 20 mẫu look-around viết lại thành bộ lọc hậu kiểm ("khớp X
và không khớp Y") là xong — rẻ hơn nhiều so với rước engine backtracking vào.

Mẹo: dùng `RegexSet` lọc thô hàng nghìn mẫu trong một lượt trước khi chạy capture.

### 12.4 Nhận diện công nghệ web

Wappalyzer **đã đóng mã nguồn từ 2023-08-23** (repo gốc 404, npm deprecated).
Bản fork còn sống: **`enthec/webappanalyzer`** — **7 540 công nghệ**, commit trong
tuần này, nhưng **GPL-3.0 (lây)**. Nếu không muốn dính GPL thì dùng
`nuclei-templates` (**MIT**, 904 template công nghệ).

**Không có crate Rust nào dùng được** — `wappalyzer` bỏ hoang từ 2020, các crate
mới đều dưới 5 sao. Tự parse JSON là đường đúng.

Phát hiện đáng giá nhất: **mẫu `dom` KHÔNG cần trình duyệt.** Với HTML render sẵn
ở server, `dom` chỉ là selector CSS. `scraper` parse được **100%** trong 1 787
selector. Hỗ trợ `dom` đưa độ phủ từ **79% → 88%** mà không cần headless —
**cao hơn cả `wappalyzergo`**, thư viện Go dẫn đầu hiện nay (nó bỏ hẳn `dom` và `js`).
Phần còn lại ~12% là `js`/`xhr`/`css`, mới thật sự cần máy ảo JS.

### 12.5 Bẫy giấy phép — đọc kỹ trước khi nhúng

| Thứ | Giấy phép | Hệ quả |
|---|---|---|
| Tệp dữ liệu nmap (`nmap-service-probes`, `nmap-os-db`, `nmap-services`) | **NPSL** — *không* phải GPL, không được OSI công nhận | **Không được nhúng vào sản phẩm thương mại.** Gọi nmap do user tự cài thì khác hẳn với việc phát hành kèm dữ liệu của nó |
| crate `pistol` 4.0.18 | khai MIT/Apache **nhưng đóng gói kèm 2.5 MB `nmap-service-probes` NPSL** | Xung đột giấy phép chưa giải quyết — thêm vào `Cargo.toml` là kéo NPSL vào sản phẩm |
| `rustscan` | đổi sang **GPL-3.0-only** từ 2.4.1, lại chỉ có binary (không `[lib]`) | Không dùng làm thư viện được |
| **JA4S** và toàn bộ họ JA4+ | **FoxIO License 1.1** — cấm thương mại hoá | Đúng cái mình cần (vân tay *server*) lại là cái bị hạn chế |
| **JARM** | **BSD-3-Clause** | → dùng JARM (`rust_jarm`) thay JA4S |
| `nuclei` + `nuclei-templates` | **MIT cả hai** — đã đọc trực tiếp hai tệp LICENSE, **chưa từng có sự kiện đổi giấy phép**, **không có tầng template "pro"** | Sạch, là đích gọi-ngoài hợp lý nhất. (Tin đồn "đổi giấy phép" nhiều khả năng bắt nguồn từ việc nuclei thêm **ký số template** — template `code:`/`javascript:` tự viết phải ký, đó là ràng buộc kỹ thuật chứ không phải giấy phép) |
| `enthec/webappanalyzer` | **GPL-3.0** (lây) | Cân nhắc nếu phát hành kèm |
| `sslyze` | **AGPL-3.0 — đã xác nhận, KHÔNG có ngoại lệ nào** | Câu "see LICENSE for exceptions" trong README chỉ trỏ vào boilerplate §7 của AGPL. Không có giấy phép thương mại chào sẵn |
| `nikto` | code GPL-3.0 nhưng **CSDL độc quyền**, URL chính sách 404 | **Loại** — xem §12.6 |

Chi tiết đầy đủ về từng công cụ ngoài: **§12.6**.

### 12.6 Công cụ ngoài — cái nào bọc được, cái nào tuyệt đối không

Nhắc lại: máy này **không có công cụ nào** trong số dưới đây. Chúng là lớp làm giàu
tuỳ chọn, phát hiện lúc chạy — không phải điều kiện bắt buộc.

| Công cụ | Giấy phép | Bọc? | Lý do |
|---|---|---|---|
| **nuclei** v3.11 | **MIT** cả engine lẫn template | **CÓ — công cụ neo** | ~13 200 template, **1 496 gắn thẻ KEV**, JSONL sẵn CVSS+EPSS+CWE. 136 MB |
| **httpx / dnsx / katana** | MIT | CÓ | httpx 65 trường làm giàu; katana dùng **`-jsonl`** (không phải `-json`) |
| **osv-scanner** v2.4 | Apache-2.0 | CÓ | Offline qua HTTPS thuần từ GCS, tải theo từng hệ sinh thái; module v2 semver thật |
| **syft / grype** | Apache-2.0 | CÓ | grype **sẵn EPSS + cờ KEV + điểm rủi ro tổng hợp**. ⚠️ **phải ghim ≥ v0.88.0**, schema v5 đã EOL 06/3/2026 — dưới mốc đó là quét bằng CSDL đóng băng mà không báo |
| **trivy** | Apache-2.0 | CÓ | Rộng nhất; **phải tự mirror CSDL** — hạ tầng công cộng của họ từng sập vì quá tải |
| **ZAP** | Apache-2.0 (Checkmarx bảo trợ) | CÓ, qua Docker | DAST nghiêm túc duy nhất; finding có risk+confidence+CWE. Bản cài **không ký số** |
| **testssl.sh** | GPL-2.0 | CÓ | ⚠️ **không có OpenSSL bản ARM64** → trên Apple Silicon mất phần cipher/giao thức cũ. Càng củng cố lý do tự gửi ClientHello (§12.2) |
| **sslscan** | GPL-3.0 | CÓ | 178 kB, đối chứng rẻ. Chỉ có XML |
| **sslyze** | **AGPL-3.0, KHÔNG có ngoại lệ** | Cân nhắc | Schema tốt nhất (pydantic), nhưng AGPL quyết định tất cả. Câu "see LICENSE for exceptions" trong README là **gây hiểu nhầm** — đó chỉ là boilerplate §7 của AGPL |
| **nmap** | **NPSL v0.95** | **Chỉ khi user tự cài** | Xem dưới |
| **nikto** | GPL-3.0 code, **CSDL độc quyền** | **KHÔNG** | Xem dưới |
| **rustscan** | GPL-3.0-only | **KHÔNG** | **Không có đầu ra JSON** (chỉ `--greppable` in bằng `println!`), mà lý do tồn tại của nó là bàn giao sang nmap |

**nmap — điều khoản NPSL §3 mô tả đúng cái app này đang định làm.** Nguyên văn, phần
mềm bị coi là tác phẩm phái sinh nếu nó *"được thiết kế riêng để **thực thi** phần mềm
được bảo hộ **và phân tích kết quả**"*, hoặc *"đọc/nhúng tệp dữ liệu của nó như
`nmap-os-db` hay `nmap-service-probes`"*, và có hẳn một gạch đầu dòng đóng đường vòng:
*"thực thi một chương trình/script trung gian để làm bất kỳ điều nào ở trên"*.
Tác phẩm phái sinh phải phát hành theo NPSL+GPL **kèm mã nguồn**. Giấy phép OEM khởi
điểm **59 980 USD**.
→ **Không bao giờ đóng gói kèm, không tự tải về, không tự đọc tệp dữ liệu của nó.**
Chỉ gọi bản do người dùng tự cài. Nếu có host dịch vụ quét thì §6 của NPSL chỉ đòi ghi
nhận công + link nmap.org, không đòi mở mã.

**nikto — chặn ở giấy phép, không phải ở kỹ thuật.** Nguyên văn trong `COPYING`:
*"Tệp cơ sở dữ liệu **KHÔNG** theo GPL và chỉ được phân phối như một phần của gói Nikto
chính thức"*; header của `db_tests` còn chặt hơn: *"không được dùng với bất kỳ sản phẩm
phần mềm nào nếu không có văn bản cho phép"*. Mà **cơ sở dữ liệu chính là scanner**.
Tệ hơn: URL chính sách thương mại mà `COPYING` trỏ tới (`cirt.net/Nikto-Licensing`)
**trả 404** — chính sách không tồn tại công khai. Bỏ hẳn; phần quét thụ động của ZAP
phủ gần hết cùng phạm vi mà có severity/CWE đàng hoàng dưới Apache-2.0.

#### ⚠️ Hai vấn đề của nuclei phải xử lý TRƯỚC khi bật

**1. `intrusive` KHÔNG nằm trong danh sách loại trừ mặc định.** `.nuclei-ignore` chỉ
loại `dos`, `local`, `fuzz`, `bruteforce`, `txt-service`. Nghĩa là **526 template gắn
thẻ `intrusive` chạy mặc định**, cùng với 975 `rce`, 595 `sqli`, 1 411 `xss`, 337
`default-login`, 247 `token-spray`. Với app "không có lớp khai thác" thì mặc định phải là:

```
-etags intrusive,dos,fuzz,bruteforce,token-spray,default-login -lna -duc -ni
```

và muốn bỏ chặn thì phải qua một cổng đồng ý có ghi log.

**2. OAST rò tên miền mục tiêu ra máy chủ bên thứ ba — và đây là vấn đề PHÁP LÝ.**
358 template dùng phát hiện out-of-band, mà máy chủ Interactsh mặc định
(`oast.pro, oast.live, oast.site, oast.online, oast.fun, oast.me`) **do ProjectDiscovery
vận hành**. Khi một template OAST kích hoạt, **hệ thống của khách hàng sẽ phân giải một
tên miền do bên thứ ba kiểm soát** — tên host và thời điểm rò ra ngoài.

→ Ghép với §2.3: Nghị định 356/2025 phạt tới **5% doanh thu năm trước** cho chuyển dữ
liệu cá nhân ra nước ngoài trái phép. Cách xử lý: **`-ni`** (tắt hẳn OAST) hoặc tự dựng
Interactsh trong nước. Không được để mặc định.

**3. Mọi công cụ ProjectDiscovery đều "gọi về nhà" mỗi lần chạy** — gửi OS, kiến trúc,
phiên bản, và **`machine_id`: một vân tay thiết bị ổn định**. `-silent` **không** tắt
được; chỉ `-duc` mới tắt. Client kiểm tra cập nhật còn đặt `InsecureSkipVerify: true`.
→ Luôn truyền `-duc`, **rửa sạch biến môi trường `PDCP_*` và `ENABLE_CLOUD_UPLOAD`** khi
spawn tiến trình con, và sandbox `HOME` để `~/.pdcp/credentials` không lọt vào.

#### Ba ghi chú kỹ thuật khi bọc

- **SARIF là lớp hợp nhất.** nuclei, trivy, grype, osv-scanner và ZAP đều xuất SARIF →
  chuẩn hoá một lần thay vì viết bảy bộ parser. (testssl.sh, sslscan, masscan thì không —
  phải viết adapter riêng.)
- **Giả định đầu ra nào cũng có thể bị cắt cụt.** nuclei JSONL an toàn vì theo dòng,
  nhưng **XML của nmap, JSON của masscan, testssl.sh và nikto đều ghi dấu đóng ở lời gọi
  cuối** — bị ngắt giữa chừng là hỏng cả tệp. Ưu tiên JSONL ở mọi chỗ có lựa chọn.
- **Vận hành CSDL lỗ hổng mới là chi phí ẩn.** grype 139 MB `.tar.zst`/ngày, npm của OSV
  riêng đã 213 MB, template nuclei 18 MB với 2–3 bản/tháng. Hiện chưa cái nào đòi xác
  thực, **nhưng đừng kéo từ endpoint công cộng theo từng lần quét** — mirror, cache, ghim.

### 12.7 Ghi chú kỹ thuật khi quét cổng

- Mỗi kết nối đang mở = 1 file descriptor. **macOS mặc định chỉ 256**, Linux ~1024.
  Nâng bằng crate `rlimit`, đừng cho rằng có sẵn chỗ.
- `SO_LINGER = 0` (`socket2::set_linger(Some(Duration::ZERO))`) gửi RST khi đóng,
  bỏ qua TIME_WAIT — mẹo chuẩn của scanner.
- Phân loại **ba** trạng thái chứ không phải hai: `Ok` = mở, `ConnectionRefused` =
  đóng, `Elapsed` = bị lọc.
- Cổng tạm: macOS 49152–65535 (16 384), Linux 32768–60999 (~28k). Nhiều-cổng-một-host
  thì không sao; nhiều-host-một-cổng mới là chỗ cạn.

## 13. Phụ lục A — nhật ký kiểm chứng

Chi tiết lệnh và kết quả thô: `verified-findings.md` trong scratchpad phiên này.
Tóm tắt nguồn dữ liệu đã thử sống ngày 2026-07-31:

| Nguồn | Kết quả |
|---|---|
| CISA KEV | HTTP 200, 1656 CVE, `dateReleased 2026-07-29` |
| EPSS (FIRST) | HTTP 200, CVE-2021-44228 → `epss 0.99999` |
| OSV `/v1/query` | HTTP 200, Debian/Ubuntu/npm/Maven đều có dữ liệu |
| OSV `/v1/querybatch` | 100 truy vấn / **0.76 s** |
| crt.sh | **HTTP 502** |

Công cụ ngoài trên máy này: `nmap ✗ nuclei ✗ testssl.sh ✗ trivy ✗ nikto ✗
sslyze ✗ httpx ✗ subfinder ✗ naabu ✗ lynis ✗`, chỉ có `openssl ✓` và `dig ✓`.
→ Lõi app phải tự chủ bằng Rust + HTTP; công cụ ngoài chỉ là lớp làm giàu tuỳ chọn.

## 14. Còn chưa rõ — cần xác minh trước khi code

Ghi rõ để không ai tưởng phần này đã chốt.

### Đã giải quyết trong phiên này

Bảng hệ số Observatory (§8.1), rubric SSL Labs (§8.2), OWASP Top 10:2025, Lynis,
**pháp lý Việt Nam** (§2.1–2.3), **quy trình xác minh của scanner thương mại** (§6.2),
và **toàn bộ khảo sát công cụ ngoài** (§12.6) — **đều đã lấy được**.
`nuclei -jsonl` cũng đã xác minh từ mã nguồn: trường `info` được **inline** (không lồng
dưới khoá `info`) — đây là lỗi parser phổ biến; chỉ `template-id`, `type`, `timestamp`,
`matcher-status` là luôn có, còn lại phải mô hình hoá bằng `Option<T>`. Lynis: **GPLv3**, agentless, shell POSIX thuần, không cần cài;
đầu ra máy đọc ở `/var/log/lynis-report.dat` với cú pháp `option=value` và
`option[]=value` cho mảng; parse các khoá `hardening_index=`, `warning[]=`,
`suggestion[]=`, `vulnerable_packages_found=`. Chạy `lynis audit system --cronjob --quiet`.
⚠️ **Đừng trình bày `hardening_index` như điểm bảo mật** — CISOfy nói rõ nó
"chỉ là chỉ báo về các biện pháp đã áp dụng" và **không so sánh được giữa các máy**.

Đáng cân nhắc thêm: **`ssh-audit`** (**MIT**, bản 3.9.0) — nó là **scanner mạng, không
phải bộ đọc file cấu hình**, tức là báo cáo đúng những gì daemon *thực sự* chào ra.
Có `-j/--json`, mã thoát `0/1/2/3`, và sẵn policy cho Debian 12/13, Ubuntu 20.04–26.04.
Bọc cái này rẻ hơn tự viết phần lớn phép kiểm SSH ở §11-L3.

### Còn treo thật

| Mục | Vì sao |
|---|---|
| **Ba câu hỏi pháp lý VN còn lại** | Khung chính đã xác minh (§2.1–2.3), nhưng ba thứ phụ thuộc vào 8 tuần vừa rồi: (a) **Nghị định 53/2022 có còn hiệu lực không** sau khi luật mẹ bị bãi bỏ — Điều 45 im lặng; (b) hai nghị định hướng dẫn Luật 116/2025 đã ban hành chưa; (c) khung xử phạt cụ thể trong Nghị định 174/2026 cho hành vi quét trái phép. **Cần luật sư trước khi phát hành ra ngoài** |
| **`/api/llm-config` có lộ API key không** | Thao tác đọc bị chặn, tôi không lách — anh tự kiểm |
| **nmap NPSL với ảnh Docker/OCI** | §3 có ngoại lệ hẹp cho "dạng nén/lưu trữ". Lớp OCI là tar+gzip nên đọc theo nghĩa đen thì có vẻ lọt — **nhưng không có tuyên bố chính thức, FAQ hay tiền lệ nào**, mà gạch đầu dòng về installer cho thấy ý đồ soạn thảo đi hướng ngược lại. **Câu hỏi pháp lý mở quan trọng nhất nếu đóng gói container** |
| **Điều khoản CSDL của trivy/grype** | Công cụ Apache-2.0, nhưng dữ liệu tổng hợp từ NVD/GHSA/vendor mỗi nguồn một điều khoản. Issue hỏi đúng câu này bị **đóng vì quá hạn, không ai trả lời**. Mirror để tự quét thì an toàn hơn hẳn phát hành lại |
| **Giấy phép EPSS** | Không có văn bản chính thức trên FIRST lẫn Empirical Security — chỉ có tuyên bố "freely and openly accessible" |
| **`httpx -tech-detect`** | Được cho là dùng `wappalyzergo` nhưng chưa đọc mã xác nhận |
| **Ánh xạ `Deferred` ↔ "Not Scheduled" của NVD** | Suy luận mạnh từ dữ liệu sống, không có văn bản NIST nói thẳng |
| **Thời gian chạy testssl.sh mỗi host** | Man page không nói; phải tự đo. Tính bằng phút chứ không phải giây |

### Ba thứ sẽ âm thầm làm hỏng công cụ nếu bê giả định 2024 sang

1. **BOD 22-01 đã bị thu hồi.** Mọi logic cứng kiểu "trong KEV ⇒ vá trong 14 ngày" là sai.
2. **CVSS v4.0 không có công thức** — nó tra bảng MacroVector. Không tự cài lại được
   như v3.1, và ~74% CVE 2025 **không có** điểm v4.
3. **API của securityheaders.com đã đóng tháng 4/2026.** Không dùng để đối chiếu tự động
   được nữa (trang web thì vẫn chạy, nhưng **403 với client không phải trình duyệt**).
   → Cài theo MDN Observatory, và nhớ giá trị của nó **khác** bản Mozilla cũ.
