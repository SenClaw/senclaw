# laya-browser v19s: tải bằng một nút, format v5, và CUDA

Ngày 2026-10-01 · Yêu cầu: "thay vì chọn laya-browser thì hỗ trợ button tải luôn laya-browser về, và nghiên cứu khi chạy hỗ trợ cuda" · Người dùng chọn: đăng bản ONNX lên **HF của họ**, dùng **v19s mới nhất**.

## Tóm tắt

| Phần | Trạng thái |
|---|---|
| Xuất ONNX v19s (`cklxx/laya-browser@645cf366`) | Xong: `Laya-jev/models/laya-browser-v19s`, 1.325 MB, parity max \|Δp\| = 1,0e-4 / 26 câu trả lời, mọi choice trùng |
| Daemon nói format v5 của v19s | Xong, commit `8dc6867` (nhánh `feat/laya-browser-v19s`) |
| Sửa vòng lặp: bước bị runtime từ chối không còn tính là "đã thử" | Xong, commit `5aabdbc` |
| Nút "Tải về" ở Settings → Browser (web, desktop) | Xong: web `fd48d87`, desktop `e23b18d` (nhánh `feat/laya-browser-download`) |
| Mục `laya-browser` trong catalog `sen-sysone` | **Chờ** repo HF: cần commit sha sau khi upload |
| Upload lên Hugging Face | **Chờ** người dùng `hf auth login` (token write) |
| Nghiên cứu CUDA | Xong: [research-261001-1730-sen-sysone-cuda.md](research-261001-1730-sen-sysone-cuda.md) |

## Vì sao phải tự đăng bản ONNX

`cklxx/laya-browser` chỉ có PyTorch (`model.safetensors` 644 MB, bf16). `ddoice/laya-browser-onnx` là repo rỗng. Không có bản ONNX nào khác của laya-browser trên Hub, nên nút Tải cần một repo ta đăng: bản export kèm `manifest.json` (sha256 từng file), `source.json` (commit gốc) và model card (Apache-2.0, ghi nguồn cklxx + Laya).

## v19s khác v10s ở đâu (format v5)

Đọc từ `laya_browser.py`, `code/finetune/common_ft.py` và `code/apps/systemone_server.py` ở commit `645cf366`. Ba nguồn có thân hàm giống nhau.

| | v3 (v10s) | v5 (v19s) |
|---|---|---|
| Dòng option | `[3] Search (searchbox) = 'json'` | `Search (searchbox) = 'json'` (bỏ `[index]`, laya tự viết key) |
| Option dropdown | role + value lặp ở mọi option | chỉ `Field → Option`, cắt giữ tên option |
| State | `page`, `recent_actions` | **`fields`** (≤ 14 ô kèm giá trị hiện tại) + `page` + `recent_actions` |
| Enter | không có (LLM lo) | `PRESS_ENTER` "Press Enter in the focused text field (submit it)", chỉ khi ô đang focus có chữ |
| Choice > 60 option | không chia (budget cắt bớt) | chia xen kẽ, các winner thi lại ở lượt 2 (`predict_chunked`) |

Phiên bản laya: cklxx train bằng laya 0.3.4. `sen-sysone` port 0.3.20. Đã diff `build_sequence` / `render_options` giữa hai bản: với choice thì tương đương (0.3.20 chỉ thêm nhãn noul tuỳ chỉnh và dùng lại `state_ids`).

## Code (daemon)

- `encoder.rs`: `Profile::LayaV5`, `compact_v5`, `fields_summary`, `cut` (giống `_cut`), `strip_index`, `py_strip`/`py_split` theo `str.isspace` của Python. `py_repr` nay escape như Python (NBSP `\xa0`, `​`, ` `…). Alias `PRESS_ENTER` → `KEY_ENTER` qua `Encoded::aliases` + `loop_names`, để tầng risk vẫn thấy `KEY_ENTER`.
- `decide.rs`: `ask_chunked` (`MAX_OPTIONS_V5 = 60`); `resolve` đổi tên trả lời về tên của loop.
- `ports.rs` / `settings.rs`: `Decider::local_profile` đọc `laya_fmt` trong `rl_agent_config.json` của checkpoint đã cài; thiếu field thì dùng v3.
- `policy::select_backend` nhận profile của checkpoint local; `run.rs` log `local steps by … request format …` mỗi task.
- `testdata/laya_v5.json`: 5 ca (search, form, feed tiếng Việt, 16 ô, 130 link) sinh bằng chính `laya_browser.py` (`gen-laya-v5-fixture.py`). Encoder phải khớp từng byte. Ca 130 link còn phát lại `predict_chunked` với một model giả xác định (2 lượt, 3 chunk 44/43/43).

## Nút Tải

Web và desktop: khi `decisionModel.installed = false` và catalog của runtime quyết định có id đó, cảnh báo hiện nút **Tải về (~1,29 GB)**, rồi tiến độ (`x / y · file`), nút Huỷ, Thử lại khi lỗi. Tải xong thì cảnh báo tự mất. Nếu catalog không có id đó thì giữ link Settings → Decision; nếu runtime quyết định chưa cài thì link Settings → Runtime. Poll `/api/decision/models` mỗi giây, chỉ khi job đang chạy.

## Kiểm thử

| Kiểm | Kết quả |
|---|---|
| `cargo test --lib` (daemon) | 2708 qua, 0 lỗi, 9 ignored (như trước) |
| Test mới | `laya_v5_matches_the_requests_the_checkpoint_was_trained_on`, `laya_v5_offers_enter_by_its_trained_name_only_in_a_field_holding_text`, `a_wide_choice_is_asked_in_chunks_whose_winners_compete`, `steps_are_asked_in_the_format_the_checkpoint_was_trained_on`, `a_checkpoint_names_the_request_format_it_was_trained_on`, `a_refused_step_is_not_counted_as_tried` (fail khi bỏ phần sửa) |
| Acceptance với v19s | **59/59** (E1–E4 Chrome thật) |
| E4 (feed, like) với v19s | like: 1 click, xong; chạy lại: 0 bước (không bỏ like); feed chậm mở trong 689 ms |
| Web | `tsc -b` + `vite build` xanh |
| Desktop | `flutter analyze` sạch; 352 test qua (2 widget test mới: luồng tải, thiếu runtime) |

### Site thật (managed Chrome headless, LLM text = Qwen3.5-2B local, ≤ 8 bước)

| Task | v10s (v3) | v19s trước sửa vòng lặp | v19s sau sửa |
|---|---|---|---|
| Wikipedia: tìm "Hanoi", mở bài | hết 8 bước, 34 s, không xong | blocked ("going in circles") | **xong, 2 bước, 9,6 s** (gõ → Enter) |
| DuckDuckGo: tìm "tokio rust" | xong, 2 bước, 2,7 s | xong, 2 bước, 5,5 s (gõ → **Enter 0,93**) | — |
| Wikipedia Vietnam → bài Mekong | unverified | blocked | hết 8 bước |

DuckDuckGo "xong" ở trang kết quả là do câu tiêu chí kiểm thử viết lỏng ("on the tokio.rs website"), không phải do model. Task Mekong không đạt vì link nằm ngoài khung nhìn, model chọn tìm kiếm, và chữ do LLM 2B viết chưa đúng.

## Phát hiện cần làm tiếp

1. **Runtime coi trang Wikipedia là stale gần như mọi lần.** Với key/scroll, `act` so toàn bộ marker (chữ hiển thị + mọi action). Gợi ý tìm kiếm hiện ra sau khi gõ là đủ làm Enter bị từ chối. Click so `page_key` + guard và cũng stale ngay thao tác đầu trên trang Vietnam. Sửa vòng lặp chỉ cho phép thử lại; mỗi lần vẫn tốn một quyết định (~0,7–1,4 s). Đề xuất: với Enter thì chỉ so guard của ô đang focus. Đây là quyết định bảo mật của runtime, chưa làm.
2. **Quyết định trên trang lớn tốn ~1–1,5 s CPU** (nhiều chunk + lượt 2) so với ~0,3 s trên fixture. Báo cáo CUDA cho thấy GPU chỉ thắng rõ ở đúng loại trang này.
3. Máy người dùng đang cài `laya-browser` v10s (import thư mục). Muốn lên v19s: xoá ở Settings → Decision rồi bấm Tải (sau khi catalog có mục mới).

## Còn chờ

1. `hf auth login`, rồi upload `Laya-jev/models/laya-browser-v19s` lên `<user>/laya-browser-onnx` (README, manifest, source kèm theo).
2. Điền repo + sha vào catalog `sen-sysone`, nâng 0.1.3, đóng gói. Docs/CLAUDE.md của `sen-sysone` thêm dòng catalog.
3. Phát hành: tag `sen-sysone` v0.1.3, `runtimes/index.json` của senclaw, push. Tất cả đều cần người dùng đồng ý.

## CUDA (tóm tắt, chi tiết ở báo cáo riêng)

Không bật mặc định. Làm spike phần cứng 0,5–1 ngày trước (RTX 30/40, request thật ≥ 1500 token). Nếu đạt cổng thì làm opt-in `Auto/CPU/CUDA` trên linux-x64/windows-x64, giữ `ort =rc.12`, tải thư viện NVIDIA (~1,6 GB) theo yêu cầu chứ không bundle. Apple Silicon (máy chính) không hưởng lợi. Lưu ý: provider prebuilt không có kernel cho RTX 50/Blackwell. Idle-clock của GPU làm một quyết định lạnh tốn 75–270 ms, ngang CPU M-series trên trang nhỏ.

## Câu hỏi chưa giải quyết

- Tên repo HF: `<user>/laya-browser-onnx` hay org?
- Có nới quy tắc stale của runtime cho Enter (phát hiện 1) không?
- Có làm spike CUDA khi có máy NVIDIA không?
