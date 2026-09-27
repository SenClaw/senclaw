# Shorebird Code Push — Design cho channel_app

> **Trạng thái: ĐỀ XUẤT ĐÓNG — chờ người dùng chốt** · nêu 2026-09-12.
> Lý do đề xuất: desktop đã có auto-update riêng, mobile phát hành qua store,
> và Shorebird là dịch vụ trả phí. Tài liệu vẫn nằm đây cho tới khi có quyết
> định; **chưa có dòng code nào**.


> Tài liệu thiết kế tích hợp OTA (over-the-air) code push bằng Shorebird cho ứng dụng Flutter `channel_app`.
> Trạng thái: **Đề xuất (research → design)**. Chưa triển khai.

## 1. Mục tiêu

Đẩy các bản sửa lỗi và cải tiến **code Dart** của `channel_app` xuống thiết bị người dùng **ngay lập tức**, bỏ qua vòng review của App Store / Play Store (thường 1–3 ngày với iOS).

`channel_app` là một **RPC-over-relay bridge** — client mỏng, mọi logic nặng (agents, MCP, memory, workflow) chạy ở daemon Rust. Vì vậy gần như **toàn bộ vòng lặp phát triển của app là code Dart** (UI Aurora, Riverpod state, session drawer, chat view, FormCard, settings…), đúng là phần Shorebird patch được → độ phù hợp **rất cao**.

## 2. Shorebird là gì

- OTA code push cho Flutter: dùng một bản Flutter engine đã được Shorebird patch; engine kiểm tra và tải bản cập nhật code Dart khi app khởi động.
- Yêu cầu: Flutter ≥ 3.24, git, tài khoản Shorebird.
- **Không cần thay đổi code app** để dùng ở mức cơ bản; chỉ đổi lệnh build.

### 2.1. Release vs Patch

| Khái niệm | Định nghĩa | Kênh phân phối |
|---|---|---|
| **Release** | Một phiên bản app cụ thể, vd `1.0.0+1` | Phát hành qua App Store / Play Store |
| **Patch** | Cập nhật OTA áp lên một release đã tồn tại | Tải nền qua Shorebird, không qua store |

Một patch **luôn gắn với đúng một release cụ thể**. Engine tải patch ở nền khi app chạy và **áp dụng ở lần khởi động kế tiếp** (mặc định — không làm chậm khởi động hiện tại).

## 3. Patch được gì / không được gì

| Patch ĐƯỢC (OTA) | Patch KHÔNG được (bắt buộc release qua store) |
|---|---|
| Business logic Dart | Native code (Kotlin/Java/Swift/ObjC) |
| UI / widget / Riverpod state | **Thêm hoặc nâng cấp plugin có phần native** |
| Code sinh ra (`app_localizations`…) | Assets mới/đổi (ảnh, font trong `assets/`) |
| Dependency thuần Dart | Nâng Flutter engine / đổi Flutter version |
| Sửa bug, đổi luồng, tinh chỉnh giao diện | Đổi permission, icon, cấu hình native |

### 3.1. Ràng buộc riêng của channel_app

`channel_app` hiện phụ thuộc các plugin **có phần native**:

`flutter_secure_storage`, `mobile_scanner`, `image_picker`, `flutter_local_notifications`, `speech_to_text`, `webview_flutter`, `sqflite`, `flutter_local_notifications`.

- **Dùng** các plugin này qua API Dart → patch bình thường.
- **Thêm mới / nâng cấp version / đổi cấu hình native** của bất kỳ plugin nào ở trên → **phải phát hành release mới qua store**, không patch được.

Quy tắc thực hành: mọi PR chạm vào `pubspec.yaml` (dependency native), `android/`, `ios/`, `macos/`, `assets/`, hay app icon ⇒ cần **release**, không phải patch.

## 4. Nền tảng hỗ trợ

| Nền tảng | channel_app dùng? | Shorebird hỗ trợ? |
|---|---|---|
| Android | ✅ (mảng chính) | ✅ đầy đủ |
| iOS | ✅ (mảng chính) | ✅ đầy đủ |
| macOS | ✅ | ✅ production-ready |
| Web | ✅ (bản web) | ❌ **không hỗ trợ** (web deploy vốn đã tức thì, không cần) |

Trọng tâm tích hợp: **Android + iOS**. macOS là bonus.

## 5. App Store compliance

Shorebird tuân thủ chính sách store vì chỉ patch code Dart, không đổi mục đích chính của app (Apple guideline 3.3.2; Google Play tương tự). Đây là cam kết chính thức của Shorebird và đã có hàng nghìn app trên store dùng. Không cần xử lý gì thêm về mặt policy.

## 6. Chi phí

| Plan | Giá | Patch installs / tháng | Ghi chú |
|---|---|---|---|
| Free | $0 | 5,000 | Unlimited apps & releases, Discord support |
| Pro | $20/th ($240/năm) | 50,000 (+$1 / 2,500) | Rollback, staging, analytics, signed patches, roles |
| Business | $400/th | 1,000,000 | Private Discord, audit logs, invoice billing |
| Enterprise | Custom | Custom | SAML, Slack riêng |

- "Patch install" = mỗi lần **một thiết bị** tải **một patch**.
- Với user base nhỏ/vừa: **Free đủ để bắt đầu**; lên **Pro** khi cần rollback + staging + analytics.

## 7. Kiến trúc tích hợp

```
Dev sửa code Dart
      │
      ├─(A) Thay đổi Dart-only ─────► shorebird patch android|ios ──► OTA ──► thiết bị (áp ở lần mở kế tiếp)
      │
      └─(B) Chạm native/asset/plugin ─► shorebird release android|ios ──► App Store / Play Store (review)
```

- `shorebird.yaml` (chứa `app_id`) được commit vào repo và tự động thêm vào `flutter > assets`.
- Mỗi patch phải build từ **đúng commit/version của release đích** → cần kỷ luật versioning (xem §9).

## 8. Kế hoạch triển khai

### Phase 0 — Chuẩn bị
1. Tạo tài khoản Shorebird, xác nhận Flutter ≥ 3.24 khớp version repo đang dùng.
2. Cài CLI: `curl --proto '=https' --tlsv1.2 https://raw.githubusercontent.com/shorebirdtech/install/main/install.sh -sSf | bash` (hoặc bản Windows).
3. `shorebird login`.

### Phase 1 — Init
4. Trong `channel_app/`: `shorebird init` → sinh `shorebird.yaml` (app_id), thêm vào assets.
5. Commit `shorebird.yaml`. Thêm `channel_app/.shorebird/` vào `.gitignore` nếu cần (state cục bộ).

### Phase 2 — Release pipeline
6. Đổi lệnh build phát hành:
   - Android: `shorebird release android` thay cho `flutter build appbundle`.
   - iOS: `shorebird release ios` thay cho `flutter build ipa`.
7. Ghép vào CI hiện có (repo đã có workflow desktop). Lưu lại **release version** để patch khớp.

### Phase 3 — Patch workflow
8. Với thay đổi Dart-only trên một release đã phát hành:
   - `shorebird patch android` / `shorebird patch ios`.
   - Chọn đúng release đích khi CLI hỏi.

### Phase 4 — (Tùy chọn) UI cập nhật chủ động
9. Thêm package `shorebird_code_push` vào `pubspec.yaml` để app tự kiểm tra patch:
   - Hiển thị banner "Đã có bản cập nhật — khởi động lại để áp dụng".
   - Hoặc ép cập nhật bắt buộc với các bản vá quan trọng.
   - Đặt logic vào một Riverpod provider, gọi ở màn hình chính.

## 9. Versioning & kỷ luật vận hành

- **Bump `version:` trong pubspec cho mỗi release** (vd `1.0.0+1` → `1.0.1+2`). Patch chỉ áp lên đúng release đó.
- **Tag git theo release** để tái tạo được commit khi cần patch (`shorebird patch` build từ working tree — phải checkout đúng commit của release + chỉ chứa thay đổi Dart cần vá).
- Duy trì một bảng ánh xạ `release version ↔ git tag` (trong CI hoặc `docs/channel-app/`).
- Khi nâng Flutter version: **phải release mới**, không patch; Shorebird engine cũng cần khớp.

## 10. Rủi ro & đánh đổi

| Rủi ro | Giảm thiểu |
|---|---|
| Engine bị Shorebird thay → phụ thuộc lịch nâng cấp của họ khi lên Flutter mới | Theo dõi changelog Shorebird; lên kế hoạch release khi nâng Flutter |
| Patch phải khớp chính xác một release | Kỷ luật versioning + git tag (§9) |
| Phần native/plugin thay đổi thường xuyên làm giảm lợi ích | Với channel_app phần native ổn định, đa số thay đổi là Dart → lợi ích cao |
| Vendor lock-in vào Shorebird | CLI/engine mã nguồn mở; có thể quay lại `flutter build` thuần bất cứ lúc nào |
| Patch lỗi đẩy ra production | Dùng Pro để có **staging** + **rollback**; test patch trước khi promote |

## 11. Quyết định đề xuất

- **Bắt đầu với Free tier**, tích hợp cho **Android + iOS**.
- Init + đổi release pipeline (Phase 1–2) trước; bật patch workflow (Phase 3) khi có release đầu tiên trên store.
- Thêm `shorebird_code_push` UI (Phase 4) sau, khi muốn kiểm soát thời điểm áp patch.
- Nâng lên **Pro** khi cần rollback / staging / analytics cho production.

## 12. Nguồn tham khảo

- Shorebird docs — Code Push: https://docs.shorebird.dev/code-push/
- Pricing: https://shorebird.dev/pricing
- `shorebird_code_push` (pub.dev): https://pub.dev/packages/shorebird_code_push
- OTA 2026 guide (DEV): https://dev.to/techwithsam/how-to-push-over-the-air-ota-flutter-updates-with-shorebird-complete-2026-guide-4d35
- GitHub: https://github.com/shorebirdtech/shorebird
</content>
</invoke>
