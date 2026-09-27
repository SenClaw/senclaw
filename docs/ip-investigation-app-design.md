# Điều Tra IP (`apps/ipscout`) — thiết kế

Space App điều tra một địa chỉ IP / máy chủ: nó **là ai** (ASN, tổ chức, netblock,
liên hệ abuse), nó **ở đâu** (địa lý, và độ tin của con số đó), **traffic đi qua đâu**
(CDN/WAF/cloud phía trước), **cổng nào đang mở**, **cổng đó là ứng dụng gì phiên bản
nào**, và **hệ điều hành là gì**.

Cổng 4710 · MCP `ipscout-mcp` · tiền tố tool `ip_`.

## Ranh giới thiết kế

Hai lớp, tách theo việc **có gửi gói tin tới mục tiêu hay không** — đây là ranh giới
kỹ thuật, không phải mức độ hoàn thiện:

| | L1 — Hồ sơ (passive) | L2 — Bề mặt (active) |
|---|---|---|
| Gửi gói tới mục tiêu | **Không** | Có (TCP connect) |
| Nguồn dữ liệu | RDAP, DNS, GeoIP, DNSBL | Chính máy chủ đó |
| Chạy được với IP bất kỳ | Có | Có |

Cả hai lớp đều **không đòi xác minh sở hữu**. App tin người dùng chủ SenClaw có
quyền với mục tiêu họ khai; trách nhiệm pháp lý về việc quét đúng mục tiêu (của
mình hoặc được uỷ quyền) nằm ở người dùng và ở lớp trên — skill `ipscout-investigate`
yêu cầu agent phải hỏi/xác nhận trước khi gọi lớp chủ động cho tên miền bên ngoài.

L2 mở kết nối TCP thật tới máy chủ. Với hệ thống của người khác đó là hành vi bị cấm
(Điều 7.5 Luật An ninh mạng 116/2025); với hệ thống của chính mình thì Điều 15.2.a
lại **đặt nghĩa vụ** phải tự kiểm tra. Không có bước xác minh cơ học ở tầng app vì
DNS TXT / `.well-known` / meta là ràng buộc phù hợp với dịch vụ SaaS (nhiều tenant
không tin nhau), không phù hợp với một công cụ chạy trên máy của chính chủ hạ tầng —
ở ngữ cảnh này bước xác minh tạo ma sát mà không thêm bảo đảm nào.

**Chốt kỹ thuật duy nhất còn tự chạy ở lớp scan:** chặn các **điểm cuối metadata cloud**
(`169.254.169.254`, Azure `168.63.129.16`, Alibaba `100.100.100.200`, `fd00:ec2::254`,
`fd20:ce::254`). Không có ca dùng hợp lệ nào cho việc quét chúng — chạm chúng nghĩa
là app bị lừa qua DNS rebinding hoặc bản ghi độc. Dải riêng bình thường (10/8,
192.168/16, 127.0.0.1) được phép, vì quét LAN của chính mình là chuyện thường.

**Không làm, và sẽ không làm:** quét SYN/stealth hay bất cứ kỹ thuật né tránh phát
hiện nào, quét dải mạng hàng loạt, dò mật khẩu, khai thác lỗ hổng, UDP flood. Đó là
ranh giới, không phải backlog. Một cổng mở được ghi nhận kèm mức rủi ro; app dừng ở
chỗ nói "Redis đang phơi ra Internet", không đi tiếp bước kết nối vào.

Giới hạn cứng chống biến thành công cụ quét hàng loạt: mỗi lần một host, danh sách
cổng tối đa 1024, đồng thời tối đa 64 kết nối, và mọi hồ sơ dựng sẵn đều nằm dưới
mức đó.

## Điều tra được những gì

### L1 — Hồ sơ

- **RDAP** (thay WHOIS: trả JSON, có bootstrap chuẩn IANA) → ASN, tên tổ chức, dải
  CIDR được cấp, quốc gia đăng ký, ngày cấp, email abuse.
- **Địa lý** — thành phố / quốc gia / múi giờ / toạ độ. Trả kèm **độ tin**: dữ liệu
  GeoIP là suy luận từ đăng ký chứ không phải đo đạc, đúng ở mức quốc gia ~95–99%
  nhưng ở mức thành phố chỉ ~55–80%, và với IP của CDN thì con số thành phố **vô
  nghĩa** — nó chỉ ra PoP gần nhất. App nói thẳng điều đó thay vì in một cái tên
  thành phố trông như sự thật.
- **PTR + xác nhận xuôi (FCrDNS)** — PTR do chủ IP tự đặt nên khai gì cũng được;
  chỉ khi tra ngược ra tên rồi tra xuôi tên đó về đúng IP ban đầu thì mới tin được.
  App phân biệt rõ hai trạng thái này.
- **DNS xuôi** của host: A/AAAA/MX/NS/TXT.
- **Traffic đi qua đâu** — nhận diện CDN/WAF/cloud đứng trước (Cloudflare, Akamai,
  Fastly, CloudFront, Google, AWS, Azure, Alibaba…) từ ASN + tên tổ chức RDAP + PTR.
  Khi có CDN, IP thấy được **không phải máy chủ gốc**, và mọi kết luận về cổng/OS
  bên dưới đều nói về CDN chứ không phải về hạ tầng của người dùng — cảnh báo này
  quan trọng hơn bản thân số liệu.
- **Tiếng tăm** — tra DNSBL (Spamhaus ZEN, SpamCop, Barracuda) bằng truy vấn DNS
  đảo octet. Vẫn không gửi gói nào tới mục tiêu.

### L2 — Bề mặt

- **Quét cổng TCP connect** theo hồ sơ (`top20`, `top100`, `web`, `db`, `remote`,
  hoặc danh sách tự khai). Bắt tay TCP đầy đủ — nghĩa là **có ghi log ở phía máy
  chủ**, đúng như một kết nối bình thường. Không giả mạo, không nửa mở.
- **Bắt banner** — đọc lời chào máy chủ gửi ra (SSH, SMTP, FTP, POP3, IMAP, MySQL,
  PostgreSQL, Redis, MongoDB…), và với mọi cổng im lặng thì một `GET /` lấy header
  `Server` (không giới hạn theo danh sách cổng HTTP cứng — dịch vụ web ở cổng lạ
  đúng là ca đáng phát hiện nhất).
- **Nhận dạng dịch vụ** — banner → sản phẩm + phiên bản.
- **TLS** — phiên bản, CN/SAN, nhà phát hành, hạn dùng. SAN thường lộ thêm tên miền
  khác cùng máy chủ.
- **Đoán hệ điều hành** — **suy luận có trọng số từ bằng chứng**, không phải vân tay
  ngăn xếp TCP/IP kiểu `nmap -O`. Vân tay ngăn xếp cần raw socket (quyền root) và
  gửi gói dị dạng; app cố tình không làm. Đổi lại nó cộng bằng chứng từ hậu tố gói
  của banner (`OpenSSH_8.9p1 Ubuntu-3ubuntu0.4` → Ubuntu 22.04), header `Server`,
  chuỗi TLS, thứ tự cổng mở — rồi trả **kết luận kèm phần trăm và danh sách bằng
  chứng đã dùng**. Với máy chủ thật cách này chính xác hơn đoán theo TTL, và quan
  trọng hơn: nó cho người đọc thấy vì sao.

## Dữ liệu — theo project, có lịch sử

```
projects  (id, name, note, created_at)
targets   (id, project_id, input, host, label, created_at)
runs      (id, target_id, layer, status, ip, started_at, finished_at, error, summary)
ports     (id, run_id, target_id, port, service, product, version, banner, severity, detail)
findings  (id, run_id, target_id, fingerprint, severity, category, title, detail, evidence, …)
```

Mỗi lần điều tra là một **ảnh chụp** (`runs`) chứ không ghi đè trạng thái hiện tại.
Vì thế trả lời được câu hỏi thật sự đáng giá: *"so với tuần trước có gì đổi?"* —
cổng nào vừa mở thêm, dịch vụ nào vừa đổi phiên bản, IP có nhảy sang ASN khác không.
`ip_diff` so hai run theo `fingerprint`.

## Nguồn ngoài

Mọi lệnh gọi ra ngoài đều qua bộ chặn SSRF của `scope.rs` (registry
special-purpose của IANA + các điểm cuối metadata cloud), và **kiểm lại sau mỗi lần
chuyển hướng** — không có ngoại lệ cho "nguồn tin cậy".

| Việc | Nguồn | Ghi chú |
|---|---|---|
| ASN | `*.origin.asn.cymru.com` (DNS) | Team Cymru, không khoá, đi qua cache resolver |
| RDAP | `rdap.org` (bootstrap IANA) | JSON, không cần khoá |
| Địa lý | `ipwho.is` **và** `ipapi.co` | hai nguồn song song để **đối chiếu chéo**, không phải dự phòng |
| DNSBL | truy vấn DNS | không có HTTP |

Địa lý gọi hai nguồn cùng lúc là có chủ đích: hai CSDL độc lập cùng nói một quốc gia
là bằng chứng mạnh hơn hẳn một nguồn nói chắc nịch, và khi chúng **không** khớp thì
đó chính là thông tin đáng báo (IP vừa đổi chủ, hoặc đi qua VPN/proxy).
