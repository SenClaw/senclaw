# Chạy `desktop_app` trên máy tính bảng Android & iPad — Thiết kế

> **Trạng thái: HOÃN** · chốt 2026-09-12 · xét lại khi có người dùng iPad thật.
> Lý do: chưa ai chạy `desktop_app` trên tablet, nên không có ràng buộc thật
> để thiết kế theo. Đây là nghiên cứu khả thi, **không phải tính năng**.


> Trạng thái: **Đề xuất (chưa code)** · Tác giả: nghiên cứu khả thi · Ngày: 2026-07-14
> Phạm vi: đưa Flutter `desktop_app/` chạy trên iPad và Android tablet ở chế độ **remote client**.

## 1. Mục tiêu & phi mục tiêu

**Mục tiêu**
- Chạy **toàn bộ** UI/feature của `desktop_app` trên iPad / Android tablet, dùng lại ~1 codebase.
- Tablet là **client**, kết nối tới một daemon `senclaw` đang chạy ở nơi khác (Mac/PC/server) qua **LAN** (giai đoạn 1) hoặc **relay** (giai đoạn 2).
- Người dùng nhập/quét địa chỉ daemon một lần, app nhớ và tự kết nối lại.

**Phi mục tiêu (v1)**
- KHÔNG chạy daemon `senclaw` (Rust) ngay trên tablet — không khả thi trên iOS, phức tạp trên Android; và không cần thiết.
- KHÔNG thay thế `channel_app` (bản mobile relay + E2E hiện có). Đây là hướng bổ sung, không loại trừ.
- KHÔNG làm điện thoại (phone) — layout nhắm tablet/màn lớn; phone để sau.

## 2. Vì sao khả thi (tóm tắt khảo sát)

| Thành phần | Kết luận |
|---|---|
| Transport | Đã tách sạch qua `ApiClient` + `WsClient`; comment ở [`api_client.dart:17`](../desktop_app/lib/core/transport/api_client.dart) tự nhận đây là "single seam" cho bản mobile. |
| Daemon supervisor | Đã có nhánh `external` (không spawn) cho web ở [`daemon_supervisor.dart:54`](../desktop_app/lib/core/daemon/daemon_supervisor.dart). Mobile đi cùng nhánh. |
| Plugin desktop-only | `window_manager`, `desktop_multi_window`, `tray_manager`, `local_notifier` — **đã bọc guard `_isDesktop`** ở [`main.dart:28`](../desktop_app/lib/main.dart) và [`app.dart:42`](../desktop_app/lib/app/app.dart). Không gọi trên mobile ⇒ build không vỡ. |
| `dart:io` (12 file) | Chạy tốt trên android/ios (chỉ web mới thiếu). Không phải blocker mobile. |
| Các plugin còn lại | `http`, `web_socket_channel`, `xterm`, `file_picker`, `record`, `audioplayers`, `fl_chart`, `flutter_graph_view`, `gpt_markdown`, `qr_flutter`, `url_launcher`, `path_provider`, `shared_preferences`, `flutter_inappwebview` — **tất cả hỗ trợ android/ios**. |

**Blocker thực sự** chỉ có 4 nhóm, đều xử lý được: (a) cấu hình host runtime, (b) guard daemon-spawn, (c) cleartext/ATS + quyền native, (d) đường thay thế cho OS notification.

## 3. Kiến trúc mục tiêu

```
┌────────────────────────┐         LAN / relay          ┌─────────────────────────┐
│  iPad / Android tablet │                              │  Mac / PC / server      │
│  ┌──────────────────┐  │   http://<host>:18788/api    │  ┌───────────────────┐  │
│  │ desktop_app       │──┼──────────────────────────────▶│  senclaw daemon    │  │
│  │  (remote client)  │◀─┼──────────────────────────────│  (UI 18788,        │  │
│  └──────────────────┘  │   ws://<host>:18789/          │  │   WS 18789)        │  │
│   host lưu ở prefs     │                              │  └───────────────────┘  │
└────────────────────────┘                              └─────────────────────────┘
```

Điểm mấu chốt: mobile **= web + cấu hình host**. Nhánh web đã "attach external daemon"; mobile chỉ khác ở chỗ host không phải `127.0.0.1` mà là địa chỉ người dùng nhập.

## 4. Nguyên tắc thiết kế

1. **Một khái niệm platform tập trung.** Thêm một helper `AppPlatform` duy nhất thay cho các biểu thức `!kIsWeb && (macOS||windows||linux)` rải rác:
   - `isDesktop` — spawn/adopt daemon, có tray/window/multi-window.
   - `isManagedDaemon = isDesktop` — nơi duy nhất được spawn binary.
   - `isRemoteClient = kIsWeb || isMobile` — attach vào daemon ngoài, cần host config.
2. **Mobile không tự spawn.** Mọi đường tới `DaemonSupervisor.start()` phải rẽ sớm như web.
3. **Host là runtime state, không phải compile-time.** Hiện `host` là `String.fromEnvironment` (cố định lúc build). Cần persist qua `SharedPreferences` và cho sửa trong app.
4. **Không xoá code desktop.** Giữ nguyên guard; chỉ mở rộng điều kiện.

## 5. Checklist theo file

### Phase 0 — Scaffold nền tảng
- [ ] `cd desktop_app && flutter create --platforms=android,ios --org com.senclaw .`
      → sinh `android/` và `ios/` mà không đụng `lib/`.
- [ ] Cập nhật `flutter_launcher_icons` trong [`pubspec.yaml`](../desktop_app/pubspec.yaml): thêm block `android:` và `ios:` (đã có `image_path`, `remove_alpha_ios: true`), chạy `dart run flutter_launcher_icons`.
- [ ] `.gitignore`: đảm bảo `ios/Pods`, `android/.gradle`, `android/local.properties`… được ignore (Flutter template lo sẵn).

### Phase 1 — Cấu hình host runtime (thay đổi cốt lõi)
- [ ] **`lib/core/config/app_config.dart`** — thêm factory đọc từ prefs + cờ nguồn:
  - Giữ `AppConfig.fromEnvironment()` làm default.
  - Thêm `AppConfig.fromPrefs(SharedPreferences)` đọc key `senclaw:remote-host` / `senclaw:remote-ui-port`.
  - Thêm getter `bool get isConfigured => host.isNotEmpty`.
- [ ] **`lib/core/prefs.dart`** — thêm hằng key: `kRemoteHostKey='senclaw:remote-host'`, `kRemoteUiPortKey='senclaw:remote-ui-port'`.
- [ ] **`lib/core/transport/connection.dart`** — `appConfigProvider` seed từ prefs trên mobile/web, từ environment trên desktop:
  ```dart
  final appConfigProvider = StateProvider<AppConfig>((ref) {
    if (AppPlatform.isRemoteClient) {
      return AppConfig.fromPrefs(ref.read(prefsProvider));
    }
    return AppConfig.fromEnvironment();
  });
  ```
- [ ] Thêm hàm lưu host: khi người dùng nhập host mới → `prefs.setString(...)` + `ref.read(appConfigProvider.notifier).state = ...` + `wsClient.updateConfig` + `apiClient.updateConfig` + `ref.invalidate(connectionBootstrapProvider)`.

### Phase 2 — Guard daemon supervisor
- [ ] **`lib/core/daemon/daemon_supervisor.dart`** — mở rộng nhánh external ([dòng 54](../desktop_app/lib/core/daemon/daemon_supervisor.dart)):
  ```dart
  Future<void> start() async {
    if (AppPlatform.isRemoteClient) {   // was: if (kIsWeb)
      _setPhase(DaemonPhase.external);
      return;
    }
    ...
  }
  ```
- [ ] **`lib/core/daemon/port_tools.dart`** — `status()` đã trả "free" trên web/windows; thêm `|| Platform.isAndroid || Platform.isIOS` cho an toàn (mobile không có `lsof`).
- [ ] **`lib/app/app.dart` `_quitApp()`** — chỉ chạy trên desktop (đã nằm trong tray menu, vốn desktop-only). Không cần đổi, nhưng xác nhận `killPort` không bị gọi trên mobile.

### Phase 3 — Startup UX cho remote client
- [ ] **`lib/core/daemon/startup_gate.dart`** — khi `isRemoteClient` và `!config.isConfigured`: hiện **màn hình nhập host** thay vì "Starting daemon…". Khi đã cấu hình nhưng không kết nối được: màn lỗi hiện host hiện tại + nút "Đổi máy chủ" (thay cho log tail daemon vốn vô nghĩa với remote).
- [ ] **`lib/features/settings/connection_screen.dart`** (mới) — form nhập `host` + `port`, nút "Kiểm tra kết nối" (gọi `GET /api/config`), nút "Lưu". Tùy chọn "Quét QR" (xem Phase 8).
- [ ] Thêm mục "Máy chủ" vào Settings để đổi host bất kỳ lúc nào.

### Phase 4 — Cấu hình native (bắt buộc, dễ quên)
- [ ] **iOS — `ios/Runner/Info.plist`**:
  - `NSAppTransportSecurity` → `NSAllowsLocalNetworking = true` (cho `http://`/`ws://` trong LAN). Nếu cần host tùy ý ngoài LAN, cân nhắc `NSAllowsArbitraryLoads` (đánh đổi bảo mật — xem §7).
  - `NSLocalNetworkUsageDescription` — chuỗi mô tả (iOS 14+ prompt quyền Local Network khi kết nối IP LAN).
  - `NSMicrophoneUsageDescription` — cho `record` (voice input).
- [ ] **Android — `android/app/src/main/AndroidManifest.xml`**:
  - `<uses-permission android:name="android.permission.INTERNET"/>` (thường có sẵn).
  - `<uses-permission android:name="android.permission.RECORD_AUDIO"/>` cho `record`.
  - Cho phép cleartext: hoặc `android:usesCleartextTraffic="true"` trên `<application>`, hoặc `network_security_config.xml` giới hạn theo domain/subnet (an toàn hơn).
  - `minSdkVersion` ≥ 21 (kiểm tra `record`/`flutter_inappwebview` yêu cầu; `flutter_inappwebview` thường cần ≥ 19–21).
- [ ] **Android — file picker/scoped storage**: `file_picker` v8 tự lo; xác nhận không cần `MANAGE_EXTERNAL_STORAGE`.

### Phase 5 — Notifications (OS push)
- [ ] `SystemNotifier` hiện chỉ `start()` trong guard `_isDesktop` ([`app.dart:59`](../desktop_app/lib/app/app.dart)) ⇒ mobile **chưa có** OS notification (chỉ có in-app bell). Chấp nhận cho v1.
- [ ] **v2 (tùy chọn):** trừu tượng hoá `SystemNotifier` thành interface, impl desktop dùng `local_notifier`, impl mobile dùng `flutter_local_notifications` (chính là package `channel_app` đã dùng — tham khảo `channel_app/lib`). Bỏ phụ thuộc `windowManager.isFocused()` ở mobile (thay bằng `WidgetsBinding.instance.lifecycleState`).

### Phase 6 — Embedded WebView cho mobile
- [ ] **`lib/widgets/embedded_web_stub.dart:14`** — mở rộng điều kiện:
  ```dart
  if (Platform.isMacOS || Platform.isWindows ||
      Platform.isAndroid || Platform.isIOS) {
    return _DesktopWebView(...);   // flutter_inappwebview hỗ trợ mobile
  }
  ```
  (Linux vẫn fallback "Open in browser".)

### Phase 7 — Responsive & cảm ứng
- [ ] Rà soát nav rail + right dock: trên tablet dùng lại được nhưng cần `SafeArea` (notch/home-indicator), tăng touch target ≥ 44pt, và ẩn thanh title-bar tùy biến (window chrome chỉ desktop).
- [ ] `lib/app/shell.dart` (hiện dùng `isMacOS` cho traffic-light padding) — chỉ áp desktop; mobile bọc `SafeArea`.
- [ ] Kiểm tra `xterm` (Terminal dock, [`right_dock.dart`](../desktop_app/lib/features/dock/right_dock.dart)): cần bàn phím ảo + font mono; cân nhắc ẩn tab Terminal trên tablet nếu UX kém.
- [ ] Landscape/portrait: khoá landscape cho tablet ở v1 (layout desktop hợp landscape hơn).

### Phase 8 — Ghép cặp bằng QR (tùy chọn, tái sử dụng)
- [ ] `channel_app/lib/screens/pairing_screen.dart` + `mobile_scanner` đã có sẵn luồng quét QR. Có thể mượn: daemon in QR chứa `{host, uiPort}` (hoặc token), tablet quét để điền form ở Phase 3 thay vì gõ IP tay.

## 6. Patch cốt lõi (tham chiếu nhanh)

**`lib/core/platform.dart` (mới) — nguồn chân lý duy nhất:**
```dart
import 'package:flutter/foundation.dart';

abstract final class AppPlatform {
  static bool get isDesktop =>
      !kIsWeb &&
      (defaultTargetPlatform == TargetPlatform.macOS ||
       defaultTargetPlatform == TargetPlatform.windows ||
       defaultTargetPlatform == TargetPlatform.linux);

  static bool get isMobile =>
      !kIsWeb &&
      (defaultTargetPlatform == TargetPlatform.android ||
       defaultTargetPlatform == TargetPlatform.iOS);

  /// Spawn/adopt a local daemon. Chỉ desktop.
  static bool get isManagedDaemon => isDesktop;

  /// Attach vào daemon ngoài, cần host config. Web + mobile.
  static bool get isRemoteClient => kIsWeb || isMobile;
}
```
→ Thay dần các biểu thức trùng lặp ở `main.dart`, `app.dart`, `shell.dart`, `mini_chat_screen.dart` bằng `AppPlatform.*` (giữ hành vi desktop y hệt).

## 7. Rủi ro & quyết định còn mở

| Vấn đề | Lựa chọn | Ghi chú |
|---|---|---|
| **Bảo mật đường truyền** | LAN cleartext (v1) vs relay+TLS (v2) | Daemon hiện bind `127.0.0.1` và **WS token optional trên localhost**. Mở ra LAN nghĩa là bind `0.0.0.0` và **bật token bắt buộc** — cần đổi ở phía Rust (`src/gateway/ui_server`). Đây là thay đổi phía server, phải quyết trước. |
| **Daemon bind address** | Thêm `SENCLAW_BIND_HOST`/flag để bind LAN | Không có thì tablet không thể kết nối. Kèm cảnh báo bảo mật. |
| **Auth** | Bắt buộc WS token + có thể thêm bearer cho `/api/*` khi không phải localhost | Hiện `/api/*` không auth (tin localhost). Mở LAN cần cân nhắc. |
| **iOS ATS** | `NSAllowsLocalNetworking` (LAN) đủ cho v1; host tùy ý cần exception rộng hơn | Tránh `NSAllowsArbitraryLoads` nếu được (App Store review soi). |
| **Phân phối iPad** | Cần Apple Developer account; TestFlight/sideload | Android tablet: APK sideload đơn giản. |
| **Terminal dock** | Ẩn trên tablet? | PTY chạy phía server, nhưng UX gõ lệnh trên tablet kém. |

## 8. Kế hoạch kiểm thử

1. **Build gate:** `flutter build apk --debug` và `flutter build ios --no-codesign` phải xanh (chứng minh guard plugin desktop-only đúng).
2. **LAN happy path:** daemon chạy trên Mac bind LAN → nhập IP trên Android tablet → StartupGate qua → Chat gửi/nhận, WS event chạy.
3. **iPad Local Network prompt:** xác nhận prompt quyền hiện đúng, từ chối → màn lỗi có nút đổi host.
4. **Mất kết nối:** rút mạng → `WsClient` reconnect backoff → phục hồi.
5. **Voice:** ghi âm (RECORD_AUDIO/NSMicrophone) → Whisper phía server → TTS playback.
6. **WebView:** mở một Space App có iframe/webview trên tablet.
7. **Regression desktop:** macOS build vẫn spawn daemon, tray, mini-window như cũ (không hồi quy).

## 9. Ước lượng công sức

| Phase | Nội dung | Effort |
|---|---|---|
| 0–3 | Scaffold + host config runtime + guard + startup UX | 2–3 ngày |
| 4 | Native config (ATS/cleartext/quyền) + build xanh 2 nền tảng | 0.5–1 ngày |
| 6–7 | WebView mobile + responsive/SafeArea/touch | 1–2 ngày |
| — | **Phía Rust:** bind LAN + bật token bắt buộc khi non-localhost | 0.5–1 ngày |
| 5, 8 | (v2) OS notifications mobile + QR pairing | 1–2 ngày |

**Tổng bản LAN chạy được: ~4–7 ngày.** Relay/TLS + notifications là gia tăng sau.

## 10. Quyết định cần chốt trước khi code
1. **Đường truyền v1:** LAN cleartext (nhanh) hay đợi làm relay/TLS luôn?
2. **Phía server:** đồng ý sửa daemon để bind LAN + bật token bắt buộc?
3. **QR pairing:** mượn từ `channel_app` ngay v1, hay nhập IP tay trước?
4. **iOS:** có Apple Developer account để test trên iPad thật chưa?
