# Check update & Auto-update cho `desktop_app` — Thiết kế

> Trạng thái: **ĐÃ LÀM** · thiết kế 2026-07-17, cài đặt xong và soát lại
> 2026-09-12. Updater GUI riêng ở [`update_desktop/`](../update_desktop),
> `swap_bundle` ở [`src/cli/commands/distrib.rs`](../src/cli/commands/distrib.rs),
> kiểm tra bản mới mỗi lần khởi động. Dòng "chưa code" cũ đọc ngược hẳn với
> thực tế — một tài liệu nói tính năng chưa có trong khi nó đang chạy cũng sai
> y như chiều ngược lại.
> Phạm vi: thêm luồng "kiểm tra bản mới → tải → cài → khởi động lại" vào Flutter `desktop_app/` (macOS / Windows / Linux).

## 1. Mục tiêu & phi mục tiêu

**Mục tiêu**
- App tự kiểm tra bản mới (lúc khởi động + định kỳ 24h), báo bằng UI **không chặn** người dùng.
- Người dùng bấm 1 nút → tải → cài → app tự khởi động lại ở bản mới.
- Tái dùng logic tải/giải nén đã có ở [`src/cli/commands/distrib.rs`](../src/cli/commands/distrib.rs), **không** viết lại bằng Dart.
- Update **cả app lẫn daemon trong một lần** (xem §4.1 — chúng nằm chung một bundle).
- Có đường lùi: cài hỏng thì rollback về bản cũ, không để user mắc kẹt với app không mở được.

**Phi mục tiêu (v1)**
- KHÔNG tự động cài ngầm. Chỉ tự động *kiểm tra*; tải và cài luôn cần user bấm.
- KHÔNG delta/patch update. Tải nguyên bundle (~vài chục MB) — đơn giản, đủ dùng.
- KHÔNG code signing / notarization. Đây là món riêng, xem §10.
- KHÔNG đụng `channel_app` (mobile) — nó update qua store/relay, khác đường hoàn toàn.

## 2. Hiện trạng

| Thành phần | Hiện trạng |
|---|---|
| CLI update | **Đã có**: `senclaw update [VERSION] [--list] [--select]` → [`update::run`](../src/cli/commands/update.rs) cập nhật binary (kiểm sha256) + web dist; từ chối thay daemon nằm trong bundle desktop. Chưa có UI nào gọi tới. |
| Cài desktop | **Đã có**: `install_desktop()` tải `SenClaw-<triple>.app.zip|.zip|.tar.gz` từ `releases/latest/download/`, giải nén (`ditto`/`zip`/`tar`), đặt vào thư mục app của OS. |
| Release assets | [`desktop.yml`](../.github/workflows/desktop.yml) job `release` đẩy toàn bộ `release-artifacts/*` lên GitHub Release theo tag `v*`. Tag có `-` ⇒ đánh dấu prerelease. |
| Version hiển thị | [`app_config.dart:2`](../desktop_app/lib/core/config/app_config.dart) — `const kAppVersion = '1.0.0'` **hard-code**, hiện ở nav rail [`shell.dart:111`](../desktop_app/lib/app/shell.dart). |
| Version thật | `Cargo.toml` = `0.2.0`; git tag mới nhất = `v0.2.0`; `pubspec.yaml` = `1.0.0+1`. |
| API version của daemon | **Chưa có**. `/api/config` ([`config_handler.rs:17`](../src/gateway/ui_server/config_handler.rs)) trả `wsPort`/`token`/… nhưng không trả version. |
| Thoát app | [`_quitApp()`](../desktop_app/lib/app/app.dart) đã đóng sub-window, `supervisor.stop()`, `PortTools.killPort(uiPort)` (giết cả daemon adopted), rồi `exit(0)`. |

## 3. Blocker #0 — version identity đang sai

Đây là **điều kiện tiên quyết**, phải sửa trước khi viết bất cứ dòng update nào:

```
Cargo.toml     0.2.0   ← khớp git tag v0.2.0 (đây là version thật của release)
pubspec.yaml   1.0.0+1 ← số vô nghĩa, không ai bump
kAppVersion    '1.0.0' ← hard-code, nav rail đang HIỂN THỊ SAI cho user
```

Hệ quả nếu cứ thế làm update: so sánh `1.0.0` (local) với `0.2.0` (latest) → semver bảo "local mới hơn" → **app sẽ báo up-to-date vĩnh viễn**. Bug này im lặng và rất khó thấy.

**Quyết định: `Cargo.toml version` == git tag == danh tính duy nhất của một release.** Mọi thứ khác phải dẫn xuất từ nó.

- `desktop.yml` truyền version vào build Flutter:
  ```yaml
  - name: Build Flutter desktop app
    working-directory: desktop_app
    run: |
      VER="${GITHUB_REF_NAME#v}"          # workflow_dispatch (không có tag) → "dev"
      [ "${GITHUB_REF_TYPE}" = "tag" ] || VER="dev"
      flutter build ${{ matrix.platform }} --release \
        --build-name="${VER:-0.0.0}" \
        --dart-define=SENCLAW_VERSION="$VER"
  ```
- `app_config.dart`:
  ```dart
  /// Bơm từ CI (`--dart-define=SENCLAW_VERSION`). Build local → 'dev'.
  const String kAppVersion = String.fromEnvironment(
    'SENCLAW_VERSION',
    defaultValue: 'dev',
  );
  bool get kIsDevBuild => kAppVersion == 'dev';
  ```
  `kIsDevBuild` ⇒ **tắt toàn bộ tính năng update** (không check, section Updates hiện "bản dev"). Tránh cảnh đang `flutter run` thì app đòi tự ghi đè chính nó.
- `pubspec.yaml` giữ `version: 0.0.0+1` như một placeholder có chú thích "CI ghi đè bằng `--build-name`" — không ai phải nhớ bump nữa.
- Thêm `"version": env!("CARGO_PKG_VERSION")` vào response `/api/config` (rẻ, app đã fetch sẵn lúc khởi động). Dùng để phát hiện lệch app↔daemon (§8.3).

## 4. Kiến trúc

### 4.1 Đơn vị update = **cả bundle**, không phải từng file

Nhìn lại bước bundle trong [`desktop.yml`](../.github/workflows/desktop.yml):

```
SenClaw Desktop.app/
  Contents/MacOS/senclaw_desktop     ← Flutter app
  Contents/Resources/senclaw         ← daemon Rust (app spawn nó làm child process)
  Contents/Resources/senclaw-media   ← sidecar speech-to-text (daemon tìm nó CẠNH chính nó)
  Contents/Resources/mlx.metallib    ← MLX cần file này NẰM CẠNH binary dùng nó
```

Bốn thứ này bị ghim với nhau: daemon nằm *trong* app, `senclaw-media` phải nằm cạnh daemon ([`media_sidecar::binary_path`](../src/media_sidecar.rs)), và `mlx.metallib` phải nằm cạnh sidecar (xem `MLX metallib bundling` — thiếu nó thì mọi lệnh STT abort). Windows/Linux cũng vậy: `senclaw(.exe)` + `senclaw-media(.exe)` nằm chung thư mục bundle.

⇒ **Thay nguyên bundle. Không bao giờ vá lẻ từng binary.** Đổi lại được một lợi ích lớn: app và daemon không thể lệch version.

#### Bundle thiếu file thì TỪ CHỐI cài, không cài rồi hỏng sau

`swap_bundle` kiểm tra bản vừa giải nén (`<bundle>.new`) trước khi đụng vào bản
đang chạy — đúng thời điểm duy nhất mà việc từ chối không tốn gì và người dùng
giữ nguyên app cũ. Danh sách bắt buộc chính là bảng trên
(`verify_bundle_payload`, có ở **cả hai** bản copy: [`distrib.rs`](../src/cli/commands/distrib.rs)
và [`update_desktop/src/apply.rs`](../update_desktop/src/apply.rs)).

Lý do phải fail chứ không warn: cả hai file đều hỏng **muộn**. Bundle thiếu
`senclaw` là app không bao giờ khởi động được; bundle thiếu `senclaw-media`
trông khoẻ mạnh hoàn toàn cho tới khi ai đó ghi âm một tin nhắn thoại vài tuần
sau và nhận `binary not found` — lúc đó không ai còn nối được hai việc với nhau.
Sidecar **không phải** Space App có quyền vắng mặt: nó ship cạnh daemon trên cả
ba nền tảng, nên thiếu = build release hỏng, phải sửa release chứ không phải cài
đè.

### 4.2 Sơ đồ

```
┌──────────────────┐  1. GET latest.json     ┌──────────────────────────┐
│  desktop_app     │────────────────────────▶│  GitHub Releases         │
│  UpdateService   │◀────────────────────────│  /latest/download/       │
│                  │  {version, assets[]}    │    latest.json           │
│                  │                         │    SenClaw-<triple>.zip  │
│                  │  2. tải asset + sha256  │                          │
│                  │◀────────────────────────│                          │
└────────┬─────────┘                         └──────────────────────────┘
         │ 3. copy update_desktop → ~/.senclaw/tmp/senclaw-updater
         │    (bundle cũ chưa có helper → fallback copy senclaw,
         │     thêm prefix `apply-update` vào args như flow cũ)
         │ 4. spawn DETACHED: senclaw-updater
         │      --staged <zip> --target <path/tới/bundle> --pid <mypid> --relaunch
         │ 5. _quitApp()  (dừng daemon, exit 0)
         ▼
┌─────────────────────────────────────────────────────┐
│  senclaw-updater (ngoài bundle, sống sót)           │
│   a. đợi pid chết (native wait, timeout 60s)        │
│   b. verify sha256                                  │
│   c. (Windows) kill process còn giữ khoá bundle:    │
│      image nằm trong bundle + msedgewebview2 có     │
│      cmdline trỏ vào bundle — toolhelp/NtQuery,     │
│      KHÔNG PowerShell                               │
│   d. giải nén → <bundle>.new                        │
│   e. rename <bundle> → <bundle>.old                 │
│      rename <bundle>.new → <bundle>                 │  ← hỏng thì rollback .old
│   f. xoá .old, relaunch app, tự thoát               │
└─────────────────────────────────────────────────────┘
```

Từ 0.4.4, updater là **`update_desktop(.exe)`** — crate standalone [`update_desktop/`](../update_desktop/)
(lockfile riêng, ngoài workspace, binary ~400 KB) nằm sẵn trong bundle
(Windows/Linux: cạnh `senclaw_desktop(.exe)`; macOS: `Contents/Resources/`).
Trên Windows nó build `windows_subsystem = "windows"` + hiện **cửa sổ mini**
("SenClaw Update", label trạng thái + progress marquee) — hết cảnh cửa sổ đen
cmd nhấp nháy, và bước dọn process không còn phụ thuộc PowerShell (máy công ty
chặn execution policy vẫn update được). Lỗi thì ghi
`~/.senclaw/tmp/update-desktop.log` + `apply-update-error.log`, hiện MessageBox
và tự khởi động lại bản cũ (swap atomic nên bản cũ còn nguyên).

Chú ý transition: updater chạy trong một lần update là updater của **bản đang
bị thay** (bản cũ). Máy đang ở ≤0.4.3 sẽ vẫn đi đường cũ (`senclaw
apply-update`) đúng một lần cuối khi lên bản có helper; từ đó về sau mọi update
đi qua `update_desktop`. Đường CLI `senclaw apply-update` giữ nguyên cho
terminal — hai bản copy logic swap (crate helper và `distrib.rs`) phải được
port tay cho nhau khi sửa bug.

Mấu chốt: **updater phải nằm ngoài bundle mà nó sắp thay**. Trên macOS process vẫn chạy được sau khi file bị xoá (inode còn giữ), nhưng Windows **khoá** file exe đang chạy — copy ra `~/.senclaw/tmp/` trước là cách duy nhất chạy đúng trên cả ba OS.

### 4.3 Vì sao không dùng GitHub API

`api.github.com/repos/.../releases/latest` cho sẵn `tag_name` + release notes, nhưng **rate-limit 60 req/giờ/IP** khi không auth — nhiều máy sau cùng một NAT văn phòng sẽ đụng trần. `distrib.rs` đã cố ý tránh chuyện này bằng URL `releases/latest/download/` (redirect, CDN, không giới hạn). Giữ nguyên quy ước đó.

## 5. Manifest `latest.json`

Job `release` trong `desktop.yml` sinh thêm một asset:

```json
{
  "version": "0.3.0",
  "publishedAt": "2026-07-17T10:00:00Z",
  "notes": "…markdown release notes…",
  "minVersion": "0.1.0",
  "assets": {
    "aarch64-apple-darwin":  { "name": "SenClaw-aarch64-apple-darwin.app.zip", "size": 84213760, "sha256": "…" },
    "x86_64-pc-windows-msvc":{ "name": "SenClaw-x86_64-pc-windows-msvc.zip",   "size": 61234567, "sha256": "…" },
    "x86_64-unknown-linux-gnu":{"name": "SenClaw-x86_64-unknown-linux-gnu.tar.gz","size": 59123456, "sha256": "…" }
  }
}
```

- `sha256` là **bắt buộc**, không phải trang trí: bundle chưa ký, checksum là lớp bảo vệ toàn vẹn duy nhất giữa GitHub và ổ đĩa user. Verify trước khi giải nén, sai thì huỷ.
- `minVersion`: bản cũ hơn mức này không update thẳng được (ví dụ đổi schema DB) → UI bảo user cài tay. Hiện chưa dùng nhưng để sẵn field còn hơn phải thêm sau.
- `releases/latest/download/` chỉ trỏ tới release **không phải prerelease** ⇒ kênh stable có sẵn, không cần code thêm. Tag `v0.3.0-beta.1` tự động vô hình với người dùng thường.
- **Bẫy đã biết** (giống `senclaw web`): `latest.json` chỉ tồn tại từ tag ≥ lần đổi workflow này. Check gặp 404 ⇒ coi như "không có thông tin", im lặng, **không** hiện lỗi đỏ.

Sinh manifest bằng bash trong job `release` (đã có sẵn toàn bộ artifact + `github.ref_name`), dùng `sha256sum` + `jq`.

## 6. Phía Rust — `senclaw apply-update`

Subcommand ẩn (`#[command(hide = true)]` — công cụ nội bộ, không phải API cho người dùng gõ tay):

```rust
/// Nội bộ: desktop_app spawn lệnh này detached để tự thay chính nó.
#[command(hide = true)]
ApplyUpdate {
    #[arg(long)] staged: PathBuf,   // file đã tải & verify
    #[arg(long)] target: PathBuf,   // bundle CẦN thay (app tự khai, xem dưới)
    #[arg(long)] pid: u32,          // đợi pid này chết
    #[arg(long)] sha256: String,
    #[arg(long)] relaunch: bool,
},
```

**`--target` do app truyền vào, không dò lại.** `install_desktop()` hiện dò thư mục cài bằng `macos_app_dir()` (probe /Applications, fallback ~/Applications). Với *update* thì sai: phải thay đúng cái bundle **đang chạy**, kể cả khi user đã kéo nó đi đâu đó. App biết chính xác vị trí:

```dart
// …/SenClaw Desktop.app/Contents/MacOS/<exe>
//   dirname → …/Contents/MacOS  →  '..' → …/Contents  →  '..' → …/SenClaw Desktop.app
String bundlePath() => Platform.isMacOS
    ? p.normalize(p.join(p.dirname(Platform.resolvedExecutable), '..', '..'))
    : p.dirname(Platform.resolvedExecutable); // win/linux: thư mục bundle
```

Đếm sai số `..` ở đây trỏ updater vào **thư mục cha của bundle** (tức `/Applications`) — nên `bundlePathFrom()` tách riêng để test được.

Refactor `distrib.rs`: tách phần giải-nén-và-đặt-chỗ của `install_desktop()` ra thành `swap_bundle(staged, target)` để cả `install desktop` lẫn `apply-update` dùng chung. `install desktop` = `download` + `swap_bundle(_, dò_thư_mục())`; `apply-update` = `wait_pid` + `verify` + `swap_bundle(_, target)` + `relaunch`.

Đổi tên tại chỗ, cùng thư mục cha (đảm bảo cùng volume ⇒ rename là atomic):

```
<target>.new   ← giải nén vào đây; hỏng ở bước này thì bundle gốc CHƯA hề bị đụng
<target>       → <target>.old
<target>.new   → <target>
xoá <target>.old
```
Nếu rename cuối thất bại → `<target>.old` → `<target>` trở lại, báo lỗi, thoát khác 0.

Relaunch: `open -a <target>` (macOS) / `Start-Process` (Windows) / `exec <target>/senclaw_desktop` (Linux).

### 6.1 Bản cài CLI: `senclaw web` phải tự tải sidecar

Desktop lấy `senclaw-media` **miễn phí** vì nó nằm trong bundle. Bản cài bằng
[`install.sh`](../scripts/install.sh) thì không: script đó chỉ tải asset
`senclaw-<triple>` và đặt vào PATH của user, nên máy đó có daemon mà **không có
sidecar nào cả** — voice chat và `/transcribe` chết ngay từ lần đầu với
`binary not found`.

`run_web` vì thế gọi `ensure_media_sidecar` trước khi start daemon:

| | |
|---|---|
| Asset | `senclaw-media-<triple>` (Windows: `.exe`) — job `build` trong `desktop.yml` upload thêm, cạnh `senclaw-<triple>` đã có |
| macOS thêm | `mlx-<triple>.metallib` → lưu thành `mlx.metallib` **cùng thư mục** với sidecar; MLX resolve theo vị trí executable nên để chỗ khác là vô nghĩa |
| Đích | `~/.senclaw/bin/` — **không** phải cạnh `senclaw`: install.sh đặt binary theo PATH của user, mà `/usr/local/bin` thường cần sudo |
| Thứ tự tìm | `SENCLAW_MEDIA_BIN` → cạnh `current_exe()` → `~/.senclaw/bin` ([`media_sidecar::binary_path`](../src/media_sidecar.rs)) — bản trong bundle **luôn** thắng bản tải về, vì sau một lần update desktop hai bản đó khác version |
| Hỏng thì sao | In warning rồi **vẫn start daemon**. Máy sau proxy vẫn có SenClaw chạy được, chỉ mất speech-to-text |
| `senclaw update` | Chỉ refresh bản trong `~/.senclaw/bin` nếu đã có. Bản trong bundle được thay nguyên cụm ở bước cài desktop — ghi đè riêng sẽ làm app mang hai version từ hai release |

**Bẫy giống `latest.json`:** asset `senclaw-media-*` chỉ tồn tại từ tag đầu tiên
build bằng workflow đã sửa. Máy chạy `senclaw web` trỏ vào release cũ hơn sẽ
nhận 404 → warning → daemon vẫn lên. Không có fallback rút sidecar ra từ
`SenClaw-<triple>.app.zip`: tải ~150 MB để lấy một binary 19 MB không đáng.

## 7. Phía Dart

### 7.1 `lib/core/update/` (module mới)

| File | Trách nhiệm |
|---|---|
| `version.dart` | Class `Version` — parse `0.3.0-beta.1`, so sánh **theo semver**. `0.10.0 > 0.9.0` (so sánh chuỗi sẽ sai). Có unit test. |
| `update_manifest.dart` | Model của `latest.json` + parse chịu lỗi (field lạ/thiếu ⇒ không crash). |
| `update_service.dart` | `check()`, `download(onProgress)`, `applyAndRestart()`. Không đụng UI. |
| `update_provider.dart` | Riverpod: `UpdateState { idle, checking, available(m), downloading(pct), ready(path), applying, upToDate, error(e) }`. |

Trạng thái lưu trong `SharedPreferences` qua `Prefs` sẵn có: `update.lastCheckAt`, `update.skippedVersion`, `update.autoCheck` (mặc định `true`).

### 7.2 Chính sách check

- Chạy lúc app khởi động (hoãn ~10s, đừng tranh I/O với lúc boot daemon) và mỗi 24h.
- Bỏ qua hoàn toàn nếu `kIsDevBuild`.
- Debounce bằng `update.lastCheckAt` — app hay bị mở/đóng liên tục (sống ở tray), không được check mỗi lần hiện cửa sổ.
- Lỗi mạng ⇒ nuốt im lặng, chỉ log. Không toast. Máy offline không đáng bị làm phiền.
- "Skip version này" ⇒ ghi `update.skippedVersion`, im cho tới bản kế.

**Cập nhật 2026-08-08 — check ở MỖI lần khởi động + popup.**
`UpdateNotifier.startupCheck()` (gọi từ `_startUpdateChecks` trong `app.dart`)
**bỏ qua** debounce 24h: câu hỏi "từ lần trước mở app tới giờ có bản mới nào
không?" chính là câu hỏi mà một lần khởi động phải trả lời, mà app này nằm ở
tray cả tuần không restart nên debounce ngày sẽ nuốt đúng cái check user để ý.
Vẫn `silent: true`, và vẫn có sàn `kUpdateStartupMinInterval` (30 phút) để một
vòng crash-restart không biến thành vòng request. Tick hàng giờ giữ nguyên
`maybeCheck()` (debounce ngày).

Ba trạng thái im lặng của **popup** (chỉ popup — chấm ở nav rail và trang
Updates luôn hiện):

| Hành động | Pref | Ý nghĩa |
|---|---|---|
| "Xem cập nhật" | — | mở Settings → Updates, không ghi gì |
| "Nhắc lại sau" | `senclaw:update:snooze-version` + `:snooze-until` | im **đúng version đó** trong `kUpdateSnoozeDuration` (24h). Bản mới hơn ra trong lúc snooze vẫn được báo — đó là lý do phải lưu kèm version. |
| "Bỏ qua bản này" | `senclaw:update:skipped-version` | im vĩnh viễn cho version đó |
| Esc / bấm ra ngoài | — | không ghi gì ⇒ lần khởi động sau hỏi lại |

Hoàn tác ở Settings → Updates (`resumeAnnouncements()`, xoá cả ba key) — nếu
không thì "Bỏ qua bản này" là cửa một chiều không dấu vết.

### 7.3 UI (4 điểm chạm)

1. **Badge ở nav rail** — chấm nhỏ trên `v$kAppVersion` ([`shell.dart:111`](../desktop_app/lib/app/shell.dart)) khi có bản mới; bấm → Settings → Updates. Đây là chỗ user vốn đã liếc để biết mình đang chạy bản nào.
2. **Settings → mục "Updates"** (thêm key `'updates'` vào `_sections` + `_UpdatesSection` trong [`settings_screen.dart`](../desktop_app/lib/features/settings/settings_screen.dart)): version hiện tại, "Kiểm tra ngay", lần check cuối, release notes (render bằng `gpt_markdown` đã có sẵn), nút "Tải & Cài" + progress, toggle auto-check.
3. ~~**Snackbar** một lần mỗi version~~ → **thay bằng popup 2026-08-08**: `UpdateAnnouncer` ([`lib/core/update/update_announcer.dart`](../desktop_app/lib/core/update/update_announcer.dart)) bọc `AppShell`, hiện `AlertDialog` một lần mỗi version với version + release notes và ba lựa chọn **Xem cập nhật / Nhắc lại sau / Bỏ qua bản này**. Toast 8 giây trên một máy vừa boot đúng là loại thông báo user không kịp thấy; ba đường thoát là thứ giữ cho một modal không thô lỗ. Bọc ngoài shell (không nằm trong) để popup sống qua điều hướng và pump được riêng trong test.
4. **Menu macOS → "Check for Updates…"** — đúng chỗ trong ảnh chụp màn hình. Thêm item vào `macos/Runner/Base.lproj/MainMenu.xib`, gửi qua MethodChannel `senclaw/app` (channel này đã tồn tại cho `activate`) một method mới `checkForUpdates` → Dart điều hướng tới Settings → Updates và trigger check.
   > Tiện thể: item **"Settings…"** trong ảnh đang xám vì xib template Flutter nối nó vào hư không. Nối nốt vào `/settings` — sửa 1 dòng, hết một món khó chịu.

### 7.4 Luồng cài

```dart
Future<void> applyAndRestart(File staged, String sha256) async {
  final updater = await _copyUpdaterOutsideBundle();   // ~/.senclaw/tmp/senclaw-updater
  await Process.start(
    updater.path,
    ['apply-update',
     '--staged', staged.path,
     '--target', bundlePath(),
     '--pid', '$pid',
     '--sha256', sha256,
     '--relaunch'],
    mode: ProcessStartMode.detached,                   // sống sót qua exit(0)
  );
  await ref.read(appProvider).quitApp();               // tái dùng _quitApp() sẵn có
}
```

`_quitApp()` dùng lại được nguyên vẹn và giải quyết luôn ca daemon **adopted**: nó đã gọi `PortTools.killPort(uiPort)` nên daemon do `cargo run` khởi động cũng bị dọn. Relaunch xong app spawn daemon mới từ bundle mới — không còn daemon cũ lảng vảng.

## 8. Ca biên & rủi ro

| Ca | Xử lý |
|---|---|
| **Bundle không ghi được** (`/Applications` do admin khác cài) | Thử `File(target/.probe).create()` **trước khi tải**. Hỏng ⇒ báo "cần quyền admin, chạy `senclaw update` trong terminal", đừng tải xong mới chết. |
| **Daemon adopted / dev** (`phase == adopted`) | `_quitApp()` giết theo port ⇒ vẫn đúng. Nhưng nếu `kIsDevBuild` thì đã tắt update từ đầu. |
| **Đang có agent chạy dở** | Hỏi xác nhận trước khi cài: "Đang có N phiên hoạt động, cài đặt sẽ dừng chúng." Lấy số từ `agentStatesProvider`. |
| **Updater chết giữa chừng** | Lần khởi động sau, app thấy `<bundle>.old` còn sót ⇒ dọn. Thấy `<bundle>` mất mà `.old` còn ⇒ rollback. |
| **App bị xoá lúc updater đang chạy (macOS)** | An toàn: updater đã được copy ra `~/.senclaw/tmp/`. |
| **Gatekeeper / quarantine** | Tải bằng `reqwest` **không** gắn cờ quarantine (chỉ trình duyệt gắn) ⇒ không có prompt "app tải từ Internet". App vẫn chưa ký — y hệt tình trạng `senclaw install desktop` hiện tại, không tệ đi. |
| **Downgrade** | Chỉ khi `latest > local`. `latest < local` (build local nhảy cóc) ⇒ coi như up-to-date. |
| **Tải giữa chừng mất mạng** | Xoá file tạm, `error(e)`, cho retry. Không resume ở v1. |
| **`latest.json` 404** (release cũ) | Coi như không có thông tin. Im lặng. |

## 9. Kế hoạch triển khai

| Phase | Việc | Ghi chú |
|---|---|---|
| **0** ✅ | Version identity: `--build-name`/`--dart-define` trong CI, `kAppVersion` từ env, `"version"` vào `/api/config` | **Xong** 2026-07-17. Chặn mọi thứ khác. |
| **1** ✅ | Sinh `latest.json` + sha256 trong job `release` của `desktop.yml` | **Xong** 2026-07-17. |
| **2** ✅ | Rust: tách `swap_bundle()`, thêm `apply-update` ẩn, `wait_pid`, verify sha256, rollback | **Xong** 2026-07-17. 13 test; e2e qua CLI thật đã verify. |
| **3** ✅ | Dart: `core/update/`, provider, Settings → Updates, badge nav rail, snackbar | **Xong** 2026-07-17. 40 test Dart (unit + widget). |
| **4** ✅ | Menu macOS "Check for Updates…" + nối "Settings…" | **Xong** 2026-07-17. xib + MethodChannel (chiều native→Dart). |
| **4.5** ✅ | Updater riêng `update_desktop(.exe)`: GUI subsystem + cửa sổ mini tiến trình, kill locker native (hết PowerShell/console flash), fallback legacy cho bundle cũ | **Xong** 2026-08-05. Crate standalone, 7 test + cross-check msvc; CI build + bundle cả 3 OS. |
| **5** | *(sau)* auto-download ngầm, kênh beta, code signing + notarization | Xem §10. **Tiếp theo.** |

Phase 0–1 có thể merge độc lập và tự nó đã có giá trị (nav rail hết nói dối version).

## 10. Phương án thay thế đã cân nhắc

**`auto_updater` (Sparkle/WinSparkle)** — cùng tác giả với `window_manager`/`tray_manager` mà repo đang dùng, nên tích hợp sẽ quen tay. Đã loại vì:
- Cần **code signing + khoá EdDSA** để hoạt động tin cậy trên macOS ≥ 13. Chưa ký thì thành đường vòng vô ích.
- **Không hỗ trợ Linux**, mà CI đang build cả ba nền.
- Không biết gì về daemon nằm trong bundle, cũng chẳng tái dùng được `distrib.rs`.
- Sparkle muốn appcast.xml + kênh phân phối riêng — trùng lặp với GitHub Releases đang xài.

Khi nào ký được app thì đánh giá lại: Sparkle cho delta update và luồng background chín hơn nhiều so với tự viết.

## 11. Test

- **Unit (Dart)**: `Version` compare — `0.10.0 > 0.9.0`, `1.0.0 > 1.0.0-beta.1`, chuỗi rác không crash. Manifest parse thiếu/thừa field.
- **Unit (Rust)**: `swap_bundle` với target giả (thư mục tạm) — thành công, giải nén hỏng (bundle gốc còn nguyên), rename cuối hỏng (rollback về `.old`).
- **Tay, mỗi OS**: cài `v0.2.0` → publish `v0.3.0` → check → cài → xác nhận app relaunch ở `0.3.0`, daemon `/api/config` cũng trả `0.3.0`, và **MLX/TTS còn chạy** (bằng chứng `mlx.metallib` đi theo bundle).
- **Ca hỏng, tay**: `chmod -w` thư mục target → phải chặn *trước khi tải*.

### Bẫy khi verify bằng tay (đã dính)

- **`flutter build macos` KHÔNG tự build lại code Dart** khi chỉ đổi `--dart-define`/`--build-name`: nó in `✓ Built` nhưng giữ nguyên `App.framework` cũ và `Flutter-Generated.xcconfig` vẫn mang giá trị mặc định. Phải `rm -rf build/macos/Build/Products/Release/App.framework` (hoặc `flutter clean`) rồi build lại. Kiểm chứng nhanh:
  ```bash
  grep FLUTTER_BUILD_NAME macos/Flutter/ephemeral/Flutter-Generated.xcconfig   # phải = version bạn truyền
  strings -a "…/App.framework/Versions/A/App" | grep -c "Install & Restart"    # phải > 0
  ```
  CI không dính vì mỗi lần build là một checkout sạch.
- **`open <đường/dẫn>.app` bỏ qua đường dẫn** nếu đã có app cùng bundle ID được LaunchServices biết tới — nó bật bản trong `/Applications`. Muốn chạy đúng bản vừa build thì exec thẳng `…/Contents/MacOS/SenClaw Desktop`.
- `App.framework/App` là **symlink** tới `Versions/A/App`; đọc mtime của symlink sẽ ra ngày sai lệch cả tháng.
