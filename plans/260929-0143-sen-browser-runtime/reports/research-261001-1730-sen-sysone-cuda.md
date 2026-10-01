# Research: sen-sysone chạy Laya trên GPU NVIDIA (CUDA)

Ngày 2026-10-01 17:30 (Asia/Saigon) · Chỉ tư vấn, không sửa code · Máy nghiên cứu: macOS arm64, **không có NVIDIA — chưa chạy CUDA lần nào**. Mọi con số GPU là trích dẫn hoặc ước lượng, có gắn nhãn.

Nhãn nguồn: **[S]** = đọc source crate/ORT, hoặc tự kiểm tra binary tải về (sha256 khớp `dist.txt`) · **[W]** = tài liệu/trang web, truy cập 2026-10-01 · **[E]** = ước lượng của tôi, có giả định. Danh sách nguồn + phiên bản ở mục 7.

## 1. Tóm tắt + khuyến nghị

**Khuyến nghị: không bật CUDA mặc định và chưa viết code sản phẩm. Làm spike phần cứng 0,5–1 ngày trước. Qua cổng thì làm opt-in `Auto / CPU / CUDA` cho linux-x64 + windows-x64, giữ pin `ort =2.0.0-rc.12`, một id `sen-sysone`.** Apple Silicon không có CUDA nên người dùng chính không được lợi gì.

Phát hiện làm đổi bức tranh:

| # | Phát hiện | Nguồn |
|---|---|---|
| 1 | rc.12 + feature `cuda` tải ORT 1.24.2 bản `cu12` (mặc định) hoặc `cu13`. Core ORT vẫn **link static** (`libonnxruntime.a`, không tham chiếu cudart/cublas/cudnn → build không cần CUDA toolkit, không cần GPU). EP CUDA là `libonnxruntime_providers_cuda.so/.dll` + `providers_shared`, ORT `dlopen` từ **thư mục chứa exe** → phải ship cạnh exe. Exe link static nên không có `NEEDED` về CUDA (suy ra từ `.a` đã quét; chưa tự build để kiểm) → vẫn chạy trên máy không NVIDIA. | S |
| 2 | Provider prebuilt chỉ chứa SASS **sm_75, sm_80, sm_90** (Turing; Ampere/Ada qua sm_80; Hopper). **Không có Pascal (GTX 10), Volta, Blackwell (RTX 50)** — cả rc.12 lẫn rc.13. pyke mới thêm sm_120 vào pipeline ngày 2026-09-23; chưa có crate nào trỏ tới. GPU không hỗ trợ: đăng ký + tạo session vẫn xong, **hỏng ở lần `run` đầu** (`no kernel image`). | S |
| 3 | "Fallback CPU im lặng" của ort chỉ bắt lỗi *dlopen provider*. Không có GPU/driver cũ: provider ném ở `commit_from_file` → `Err` cứng. Cần fallback 3 tầng + **probe run lúc nạp**. | S |
| 4 | Provider cu12 *bắt buộc* cudart, cuBLAS, cuBLASLt, cuFFT, cuRAND, cuDNN 9: ~2,5 GB giải nén / ~1,6 GB tải (Linux; Windows không cần cuRAND, ~2,2 GB / ~1,5 GB). `RUNPATH` của provider trỏ vào đường dẫn máy build (không có `$ORIGIN`) → phải preload bằng đường dẫn tuyệt đối. ORT ≥ 1.28 (rc.13) cho cuDNN/cuFFT thành optional → ~0,5 GB, nhưng rc.13 chỉ còn CUDA 13 (driver ≥ 580) và buộc nâng phiên bản cho mọi nền tảng. | S, W |
| 5 | Idle-clock penalty là của driver/phần cứng, **áp dụng cho ORT như PyTorch/TensorRT**. CUDA graph không chữa được. Request nhỏ như fixture (~500 token) → GPU lạnh ≈ CPU M-series. GPU thắng thật khi request lớn (trang thật), CPU x86 yếu, hoặc burst. | W, E |
| 6 | VRAM ~3–4 GB cho fp32 [E] → tranh với `llama.cpp-cuda` trên cùng card. | E |
| 7 | Phụ (không thuộc CUDA): gói `windows-x64` 0.1.2 **chỉ có `sen-sysone.exe`**, nhưng exe import `directml.dll` + `d3d12.dll` + `dxgi.dll` (pyke luôn build kèm DirectML trên Windows). Không ship `DirectML.dll` → dựa vào System32 (Windows 10 1903+). Gói `linux-x64` 0.1.2 cần `GLIBC_2.38` (Ubuntu 24.04+; không chạy trên Ubuntu 22.04/Debian 12). | S |

### Xếp hạng lựa chọn

| Hạng | Lựa chọn | Phán quyết |
|---|---|---|
| 1 | Spike (Python `onnxruntime-gpu` hoặc binary Rust tối thiểu) → nếu qua cổng: rc.12 static + provider libs cạnh exe, **một id**, setting `Auto/CPU/CUDA`, mặc định `CPU` ở bản đầu | **Khuyến nghị (có điều kiện)** |
| 2 | Như 1 nhưng id riêng `sen-sysone-cuda` (tiền lệ `llama.cpp-cuda`) | Dự phòng: giữ gói CPU 10–12 MB, cô lập rủi ro; đổi lại tách settings, người dùng tự chọn runtime |
| 3 | DirectML trên Windows (đã build sẵn trong binary pyke, không cần thư viện NVIDIA) | Thử **trong cùng spike**; rẻ nhất nhưng shape động có rủi ro (mục 2.7) |
| 4 | `load-dynamic` + tarball GPU chính thức của Microsoft | Chỉ khi cần Blackwell trước khi pyke ra crate mới |
| 5 | Nâng rc.13 (CUDA 13, libs ~0,5 GB) | Việc riêng, sau, kèm parity trên macOS; không phải điều kiện của CUDA |
| ✗ | TensorRT EP, CUDA graph, IO binding, export fp16 | Không làm ở v1 |

### Cổng go/no-go cho spike [E — ngưỡng do tôi đề xuất]

Trên GPU consumer ≥ RTX 3060 (CC 7.5–9.0), request **thật** (N ≥ 1500 token), nhịp 1–3 s giữa các bước, ORT CUDA fp32, `tf32=false`:

- p50 quyết định lạnh ≤ 120 ms **và** ≥ 3× nhanh hơn CPU mục tiêu cùng N;
- parity ≤ 1e-3 so với CPU;
- không cần khoá clock (`sudo`); nếu cần keep-alive thì chi phí điện trung bình < ~10 W.

Trượt cổng → không làm CUDA; xét DirectML cho Windows hoặc giảm N (xem 2.4).

### Trả lời 4 câu bổ sung (model card `cklxx/laya-browser`)

Model card đã kiểm lại: README tại commit `645cf366` (2026-09-29; HF API: `sha` = `645cf366a2ae…`), nguyên văn "22–27 ms … while the GPU is at full clock", "idle clocks (P5/P8, 210–850 MHz) within a second or two: the next decision then takes 75–270 ms", "`sudo nvidia-smi -lgc 2100,3135`", "first request of each input-length bucket also compiles a kernel once (~100–500 ms)". Đó là PyTorch + TileLang, không phải ORT. [W]

| Câu hỏi | Trả lời |
|---|---|
| 1. Idle-clock có áp dụng cho ORT CUDA EP không? | **Có.** DVFS/P-state do driver quyết định, không phụ thuộc framework. NVIDIA: "By default, the GPU clock frequency is floating, meaning it sits idle when there is no active workload…" [W]. Diễn đàn PyTorch: 13 ms → 160 ms sau khoảng nghỉ ngắn, tái hiện ở **cả PyTorch và TensorRT**, trễ nằm ở GEMM đầu tiên [W]. Khác biệt riêng của ORT [E]: (a) không có JIT-compile theo bucket (kernel dựng sẵn) nên mất khoản 100–500 ms đó, nhưng (b) fp32 làm nhiều việc GPU hơn đường bf16 đã tối ưu → cộng thêm tuyệt đối ở clock thấp có thể lớn hơn; (c) mỗi **thread** mới chạy lần đầu tạo cuBLAS + cuBLASLt + cuDNN handle [S: `cuda_execution_provider.cc` `PerThreadContext`]. |
| 2. CUDA graph / giữ GPU nóng có giúp không? | **CUDA graph: không.** Nó chỉ bỏ chi phí launch phía CPU (vài ms), không đụng clock; lại đòi shape + địa chỉ cố định, IO binding, mỗi (batch, seq) một graph (`gpu_graph_id`), lần chạy đầu chậm [W]. **Giữ nóng:** `nvidia-smi -lgc` cần root, tốn điện idle; "Prefer maximum performance" (Windows, theo app) là người dùng tự đặt; `nvidia-smi -pm 1` (persistence, Linux) chỉ giảm khởi tạo process, không giữ clock. **Heartbeat** (suy luận giả định trong lúc browser task chạy): *chưa kiểm chứng* — P-state phụ thuộc mức tải; tải quá nhẹ có thể không kéo clock lên. Phải đo duty cycle tối thiểu và công suất trong spike. Pre-warm lúc "page ready" quá muộn (request tới sau vài ms); pre-warm lúc phát lệnh act thì clock kịp tụt trong 1–2 s chờ trang [E, suy từ "within a second or two" của model card]. |
| 3. Có thắng thật mỗi bước không? | Tuỳ N (token qua encoder). Fixture: ≈ hoà. Trang thật: 3–10×. Gate/skill router: ≈ hoà. Burst/batch: thắng lớn. Bảng ở 2.4. Quan trọng: **270 ms là số trên fixture nhỏ**; CPU tăng tuyến tính theo N, GPU gần phẳng. Đo N thật bằng `usage.input_tokens` của `/v1/systemone` (có sẵn) hoặc `controlPlane.recordDecisionInputs` trước khi quyết. |
| 4. Yes / no / when | **No** làm mặc định. **Yes** opt-in thử nghiệm **khi** spike đạt cổng trên, **và** có người dùng Windows/Linux + NVIDIA thật. Không bao giờ cho macOS. Đóng góp vào thời gian cả task: một bước tiết kiệm ~0 (trang nhỏ, GPU lạnh) tới ~0,6–1,9 s (N=1500–4000) so với một lượt LLM 6–11 s (report 2026-09-30); task ~20 s, 3 bước → ~0% trên trang nhỏ, ~10–25% trên trang thật [E]. |

## 2. Kết quả theo câu hỏi

### 2.1 CUDA với ort rc.12

**Từ source crate/binary [S]**

| Mục | Kết quả |
|---|---|
| Cargo | `ort = { version = "=2.0.0-rc.12", optional = true, features = ["download-binaries", "cuda", "preload-dylibs"] }` (+ `"tensorrt"` tuỳ chọn). Default features của `ort` đang BẬT trong `sen-sysone` (`copy-dylibs`, `tracing`, `tls-native`, `api-24`). Quên `cuda`: `CUDA.register` trả `RegisterError::MissingFeature` → chỉ log warn, **chạy CPU âm thầm**. |
| API đăng ký | `ep::CUDA::default().with_device_id(0).with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested).with_tf32(false).with_memory_limit(bytes).build().error_on_failure()` rồi `Session::builder()?.with_execution_providers([cuda])?`. Dùng tên mới `ort::ep::*`; `ort::execution_providers::CPUExecutionProvider` (đang dùng trong `engine.rs`) là alias deprecated ở rc.12 và **bị xoá ở rc.13**. Mặc định ORT 1.24.2: `use_tf32=true` (comment trong ort-rs ghi "disabled by default" là sai), arena `kNextPowerOfTwo`, `cudnn_conv_algo_search=EXHAUSTIVE`, `enable_cuda_graph=false`. |
| `download-binaries` chọn dist | `ort-sys/build/download/resolve.rs`: có `cuda` hoặc `tensorrt` → `ORT_CUDA_VERSION` = 12/13; không có thì `CUDA_HOME` chứa `v13.`/`-13.`; rồi `NV_CUDA_CUDART_VERSION` bắt đầu `13.`; rồi `nvcc --version` chứa `Build cuda_13`; còn lại `cu12`. Không có dist cho tổ hợp (vd macOS + cuda): rc.12 **cảnh báo rồi lùi về `none`**; rc.13 báo lỗi link (trừ `lax-feature-matching`). Archive tải từ `cdn.pyke.io`, kiểm sha256 trong build script. CI nên đặt `ORT_CUDA_VERSION=12` cho xác định. |
| Link static? | Có. `println!("cargo:rustc-link-lib=static=onnxruntime")` cho mọi dist, kể cả cu12. Windows còn link `dxguid, DXCORE, DXGI, D3D12, DirectML`. Không có directive link CUDA nào → không cần CUDA toolkit khi build. |
| Provider lib phải ship cạnh exe? | **Có.** ORT 1.24.2: POSIX `GetRuntimePath()` = thư mục của module chứa `Env::Default` (dladdr → exe khi link static); Windows = `GetModuleFileNameW(&__ImageBase)`; provider nạp bằng đường dẫn đầy đủ; Windows dùng `LOAD_WITH_ALTERED_SEARCH_PATH`. Gói hiện tại chỉ `cp` exe, `bundle-shared-libs.sh` chỉ gom lib có trong `ldd`/`otool` (provider là `dlopen` nên không có) → phải sửa. |
| `copy-dylibs` | Symlink (Unix) / copy khi symlink lỗi (Windows) mọi `.dll/.so/.dylib` từ thư mục dist vào `target/<profile>/`, `deps/`, `examples/`. Chỉ phục vụ chạy dev; **không đóng gói**. Đã bật sẵn. |
| `load-dynamic` | rc.12: không link ORT lúc build, `ort::init_from(path)` hoặc `ORT_DYLIB_PATH`; pyke "không còn cung cấp DLL" nên phải dùng tarball chính thức của Microsoft. Một process chỉ nạp được một `libonnxruntime` → đổi CPU/GPU phải khởi động lại process. |
| `ep::cuda::preload_dylibs` | Cần `preload-dylibs`/`load-dynamic`. Danh sách cứng gồm `libnvrtc.so.12` (Linux) và **dừng ở lỗi đầu tiên** → thiếu nvrtc là bỏ dở curand/cufft. Dùng `ort::util::preload_dylib` tự lặp trên danh sách riêng. |

Nội dung archive đã tải và giải nén thử (ORT 1.24.2; sha256 khớp `dist.txt`):

| Feature | Target | Nén | File bên trong |
|---|---|---|---|
| `none` | linux-x64 | 8,7 MB | `libonnxruntime.a` 90,7 MB |
| `cu12` | linux-x64 | 60,8 MB | `libonnxruntime.a` 94,7 · `libonnxruntime_providers_cuda.so` 108,8 · `_tensorrt.so` 0,94 · `_nv_tensorrt_rtx.so` 0,76 · `_shared.so` 0,015 |
| `cu13` | linux-x64 | 59,8 MB | tương tự |
| `none` | windows-x64 | 29,4 MB | `onnxruntime.lib` 305,8 · `DirectML.dll` 18,5 |
| `cu12` | windows-x64 | 81,9 MB | `onnxruntime.lib` 323,7 · `onnxruntime_providers_cuda.dll` 96,8 · `DirectML.dll` 18,5 · `_shared.dll` · `_tensorrt.dll` · `_nv_tensorrt_rtx.dll` |
| `none` | darwin-arm64 | 8,4 MB | `libonnxruntime.a`; chuỗi `CoreMLExecutionProvider`/`MLProgram` có trong lib (EP CoreML đã biên dịch sẵn) |

Không có cu12/cu13 cho linux-arm64, windows-arm64, darwin. rc.13 (ORT 1.28.0, dist.tsv): chỉ `cuda13,tensorrt,nvrtx` (Linux) và `cuda13,tensorrt,nvrtx,directml` (Windows); không còn CUDA 12.

**Từ docs [W]**: trang `ort.pyke.io/perf/execution-providers` (footer 2026-07-28, mô tả rc.13) viết "ort provides binaries for CUDA ≥ 13.2 and targets cuDNN ≥ 9.23" — **không đúng với rc.12** (cu12/cu13, ORT 1.24.2). Trang `ort.pyke.io/setup/linking` (footer 2026-03-06): ưu tiên static; "EP CUDA/TensorRT dùng interface riêng, biên dịch thành dynamic library nạp lúc đăng ký"; `copy-dylibs` chỉ giúp dev. ORT docs: bản build CUDA 12.8 đòi libs ≥ 12.8; cuDNN 8 và 9 không tương thích; từ 1.27 gói chính thức mặc định CUDA 13 và "CUDA 12 packages are deprecated".

### 2.2 Máy người dùng cần gì

| | rc.12 `cu12` [S: DT_NEEDED, PE imports] | rc.13 `cuda13` (ORT 1.28) [S] |
|---|---|---|
| Lib NVIDIA bắt buộc (Linux) | `libcudart.so.12`, `libcublas.so.12`, `libcublasLt.so.12`, `libcufft.so.11`, `libcurand.so.10`, `libcudnn.so.9` | `libcudart.so.13`, `libcublas.so.13`, `libcublasLt.so.13`, `libcurand.so.10`, `libcuda.so.1` (cuDNN, cuFFT optional — ORT 1.28 notes [W]) |
| Lib bắt buộc (Windows) | `cudart64_12`, `cublas64_12`, `cublasLt64_12`, `cufft64_11`, `cudnn64_9` (+ 7 DLL con cuDNN, nạp lười) | chưa kiểm |
| Driver tối thiểu [W: CUDA release notes 13.4 U1] | CUDA 12.x ≥ **525** (Linux ≥ 525.60.13, Windows ≥ 527.41, minor-version compat). Đủ tính năng 12.8: ≥ 570.26 / ≥ 570.65. Khuyến nghị thực tế ≥ 570 + probe run | CUDA 13.x ≥ **580** |
| GPU | CC 7.5, 8.x, 9.0 (SASS trong provider). CUDA 13 bỏ Maxwell/Pascal/Volta [W] | như trái; sm_120 chưa có trong binary đã phát hành |
| Cần cài toolkit? | Không, chỉ cần các thư viện runtime ở trên | Không |
| Dung lượng Linux (giải nén / tải wheel) [W: PyPI, CUDA 12.9.x, cuDNN 9.27.0.42] | cudart 0,7 + cuBLAS 105 + cuBLASLt 749 + cuFFT 292 + cuRAND 167 + cuDNN 1223 = **~2,54 GB / ~1,62 GB** | cudart 0,8 + cuBLAS+Lt 603 + cuRAND 127 = **~0,73 GB / ~0,50 GB** (CUDA 13.4) |
| Windows | ~2,2 GB giải nén / ~1,5 GB tải (không cần cuRAND) | — |

Trong cuDNN 1223 MB: `engines_precompiled` 566, `adv` 275, `graph` 116, `ops` 107, `heuristic` 97, `runtime_compiled` 48. ORT 1.24.2 gọi `cudnnCreate` mỗi thread [S] nên cuDNN bắt buộc; có thể chỉ cần tập con (~320 MB) nhưng **chưa kiểm** [E] — cần LD_DEBUG/Process Monitor trên máy thật.

**Bundle có hợp pháp không** (không phải tư vấn pháp lý; cần người có thẩm quyền xem) [W]:

- CUDA EULA, Attachment A: cudart, cuFFT, cuBLAS + cuBLASLt, cuRAND, NVRTC thuộc danh sách phân phối được. cuDNN SLA: "the runtime files .so and .dll" phân phối được.
- Điều kiện: ứng dụng có "material additional functionality"; phần SDK chỉ ứng dụng của ta truy cập (để trong thư mục riêng của runtime, không cài hệ thống); điều khoản phân phối của ta phải nhất quán với EULA (cần NOTICE + hiển thị chấp nhận); bản Linux **không được sửa** (không `patchelf` lib NVIDIA — patch provider của ORT thì được); "không phân phối như sản phẩm đứng riêng".
- Tiền lệ: llama.cpp phát hành `cudart-llama-*` (cudart + cuBLAS) làm asset riêng: 391–594 MB (b11319, 2026-10-01); daemon đã tải chúng qua `extraAssets`.
- **Khuyến nghị: không bundle; tải theo yêu cầu từ nguồn của NVIDIA** (wheel `nvidia-*` trên PyPI nhỏ hơn archive redist: cuBLAS redist 944 MB so với wheel 581 MB) vào `<data_dir>/cuda/`, giữ qua các lần cập nhật runtime (package dir là bất biến theo protocol, nhúng libs vào package sẽ ép tải lại 1,6 GB mỗi bản).
- Thay thế: dùng lib có sẵn — CUDA toolkit + cuDNN, hoặc thư mục `nvidia/*/lib` của pip / `torch/lib` (ORT docs có cơ chế `preload_dlls` tương tự cho Python) qua `SEN_SYSONE_CUDA_DIR` hoặc setting.

### 2.3 Fallback

Hành vi trong rc.12 [S]:

| Tầng | Điều gì xảy ra | Kết quả mặc định |
|---|---|---|
| Đăng ký (`with_execution_providers`) | `register` lỗi khi `dlopen` provider hỏng (thiếu `providers_cuda.so`, thiếu `libcudart/cublas/cudnn…`) | `fail_silently` mặc định: `tracing::error!`, **tiếp tục CPU**. `.error_on_failure()` → `Err`. Nếu không EP nào đăng ký được: `warn!("No execution providers … registered; may fall back to CPU")` |
| Tạo session (`commit_from_file`) | `CUDAProviderFactory::CreateProvider` mới dựng `CUDAExecutionProvider` lúc khởi tạo session: `cudaSetDevice`, `cudaGetDeviceProperties`, `cublasCreate`… → không có GPU / driver quá cũ ném lỗi | **`Err` cứng**, không fallback (fallback kiểu "Failed to create CUDAExecutionProvider" nằm ở lớp pybind `onnxruntime_pybind_state.cc`, không phải C API mà `ort` gọi) |
| Chạy (`run`) | Kernel không có cho kiến trúc GPU (209 `no kernel image`), OOM, TDR/driver reset (Windows) | `Err` mỗi lần |

Và `is_available()` chỉ cho biết ORT build có EP đó, không phải dùng được. rc.12 không có API trả vị trí node; chỉ có log verbose `Node placements` của ORT, `ep::get_gpu_device()` (chỉ báo provider đã nạp) và `with_disable_cpu_fallback()` (test: ép toàn graph lên GPU).

**Thiết kế đề xuất:**

1. `local.device ∈ {auto, cpu, cuda}`. `cpu` không chạm CUDA.
2. `auto`/`cuda`: probe thụ động (build có CUDA? đúng OS/arch? GPU? CC ∈ {7.5, 8.x, 9.0}? driver ≥ CUDA 12? VRAM trống ≥ ~4 GB?) → tìm + preload lib NVIDIA → dựng session với `.error_on_failure()` → **probe run** (2 hàng × 32 token; vừa kiểm, vừa warm-up) → thành công thì `device=cuda`.
3. Lỗi ở bất kỳ bước: `auto` → dựng session CPU, ghi `device_note` (lý do) + `tracing::warn!`; `cuda` → load thất bại, UI hiện lý do.
4. Sau này `run` CUDA lỗi (OOM, TDR): hạ cấp engine xuống CPU cho phần còn lại của process (Auto) thay vì lỗi mọi request.
5. Báo thiết bị: `LoadedInfo.device` (`cpu`/`cuda`), `device_name`, `device_note`; dòng log `[decision] loaded … on cuda:0 (NVIDIA …)`; chi tiết trong `GET /runtime/info`.
6. Chạy CUDA trên **một thread riêng sống lâu**, không qua `spawn_blocking` (pool tự giết thread rảnh sau 10 s, mỗi thread mới tạo lại cuBLAS/cuDNN handle) [S/E]. Giữ `ONE_LOAD_AT_A_TIME`.

### 2.4 Hiệu năng

**Benchmark có sẵn [W]** (không có số ORT CUDA cho đồ thị này):

| Nguồn | Số |
|---|---|
| Model card laya-browser | 22–27 ms nóng; 75–270 ms sau nghỉ 1–2 s (RTX 4070 Ti SUPER, PyTorch + TileLang; N không nêu) |
| ModernBERT paper (arXiv 2412.13663, Table 2), RTX 4090 | ModernBERT-base 148,1 kTok/s ở 512 token cố định (≈ 6,8 µs/token, batch lớn). mmBERT-base cùng kiến trúc (22 lớp, 768) |
| Diễn đàn PyTorch | 13–17 ms → 160 ms sau nghỉ ngắn, PyTorch và TensorRT |
| Cũ, chỉ để biết bậc độ lớn | BERT-base seq 128 batch 1: ORT fp16 V100 1,7 ms (MS, 2020); TensorRT FP32 4,2 ms / FP16 2,1 ms (NVIDIA, GPU không nêu) |
| 4070 Ti SUPER (trang spec thứ cấp) | FP32 44,1 · TF32 dense 44 · FP16 dense 88 TFLOPS · 672 GB/s |

**Ước lượng [E]** — giả định: 0,25 GFLOP/token (110,3 M tham số lớp encoder × 2 + attention; `jhu-clsp/mmBERT-base/config.json`: ModernBERT 22 lớp, hidden 768, vocab 256k → embedding 196,6 M, tổng ~306,9 M [W]); hiệu suất sgemm ~55%; `tf32=false`; đồ thị opset 18 không fuse attention. Công thức: `T_gpu_nóng ≈ a + b·N`; `T_cpu ≈ c·N`; GPU lạnh ≈ nóng + 50–250 ms (rút từ model card: 22–27 → 75–270). N = tổng token qua encoder (mỗi câu hỏi một hàng, mỗi hàng lặp state; hàng ngắn bị pad theo hàng dài nhất). Hệ số CPU M-series suy ra từ 270 ms ↔ N ≈ 500 (**N này là giả định, chưa đo**).

| Thiết bị (fp32) | a + b·N | N=500 (fixture) | N=1500 | N=4000 (trang thật, 4 hàng) |
|---|---|---|---|---|
| Apple M CPU (c ≈ 0,54 ms) | — | 270 ms | 810 ms | 2,2 s |
| x86 8 nhân AVX2 (c ≈ 0,4–0,7) | — | 200–350 ms | 0,6–1,05 s | 1,6–2,8 s |
| x86 laptop 4–6 nhân (c ≈ 0,6–1,2) | — | 300–600 ms | 0,9–1,8 s | 2,4–4,8 s |
| RTX 4070 Ti SUPER nóng | 5 + 0,014·N | 12 ms | 26 ms | 61 ms |
| RTX 3060 nóng | 6 + 0,04·N | 26 ms | 66 ms | 166 ms |
| RTX 4070 Ti SUPER lạnh | nóng + 50–250 | 60–260 ms | 75–275 ms | 110–310 ms |
| fp16 (cần export mới) | ~4 + 0,007·N, VRAM một nửa | — | ~15 ms nóng | ~32 ms nóng |

Hệ quả: GPU entry-level (RTX 3050/GTX 1660) không nhanh hơn CPU tốt; Auto nên cần VRAM trống ≥ ~4 GB. Trên RTX 4070 Ti SUPER, TF32 dense ≈ FP32 (44 so với 44,1 TFLOPS) [W, trang spec thứ cấp] nên tắt TF32 để giữ parity gần như không mất tốc độ; các GeForce khác chưa kiểm [E]. ORT mặc định BẬT TF32 [S] → sai khác cỡ 1e-3 [E] có thể chạm ngưỡng parity 1e-3.

Workload theo ma trận:

| Workload | N | CPU M | GPU lạnh | GPU nóng | Thắng? |
|---|---|---|---|---|---|
| Bước browser, trang fixture | ~500 | ~270 ms | 60–260 ms | 12 ms | ≈ hoà |
| Bước browser, trang thật | 1500–4000 | 0,8–2,2 s | 75–310 ms | 26–61 ms | 3–10× |
| Tool-call gate (1 hàng, rời rạc) | 150–400 | 80–220 ms | 55–250 ms | 7–11 ms | ≈ hoà |
| Burst / nhiều phiên song song | bất kỳ | tuyến tính | — | 8–60 ms | 10–30× |
| Batch tới 64 hàng (`MAX_BATCH`) | 10k+ | nhiều giây | — | vài chục ms | rất lớn (chưa có workload này) |

**Tuỳ chọn CUDA EP cho shape động** [S: ORT 1.24.2; W]:

| Tuỳ chọn | Khuyến nghị |
|---|---|
| `arena_extend_strategy` | `SameAsRequested` + `gpu_mem_limit` (≈ min(trống − 0,5 GB, 3,5 GB)): mặc định `NextPowerOfTwo` làm tròn lên lũy thừa 2, đỉnh activation 6×1024 token ~0,6–1 GB bị phóng đại |
| `cudnn_conv_algo_search` | Không tác dụng: đồ thị không có `Conv` (kiểm số op `Conv` bằng script khi spike). Để mặc định |
| IO binding | Không cần: 5 tensor đầu vào ≤ ~50 KB, đầu ra nhỏ; chỉ cần nếu dùng CUDA graph |
| `enable_cuda_graph` | Không (mục 1) |
| `use_tf32` | `false` |
| `with_memory_pattern(false)` | Giữ (đã có): shape đổi mỗi request |
| `with_disable_cpu_fallback()` | Chỉ trong test/spike để biết có node nào rơi về CPU (Shape/Gather/Memcpy làm đồng bộ host–device) |
| `sdpa_kernel`/attention backend | Chỉ có tác dụng nếu graph có op `Attention` fuse; opset 18 bị phân rã thành MatMul+Softmax |

### 2.5 Đóng gói và chọn thiết bị trong SenClaw

**Hiện trạng `llamacpp.rs` [S]:** biến thể GPU là **id runtime riêng** trong `runtimes/index.json` (`llama.cpp-metal/cpu/vulkan/cuda`), mỗi id có mẫu tên asset theo platform; `cuda` Linux dùng `cuda-12.8`, Windows `cuda-12.4` + `extraAssets` (`cudart-llama-…`) giải vào cùng thư mục. **Không có dò GPU**: `accelerator` chỉ là nhãn "shown, not interpreted"; người dùng tự chọn candidate trong Settings → Runtime; tự chọn chỉ khi có đúng một candidate tương thích. `grep nvidia|nvml` trong `senclaw/src` = 0 kết quả liên quan.

| Tiêu chí | A. Một id, build có CUDA + provider cạnh exe | B. Id riêng `sen-sysone-cuda` | C. `load-dynamic` + tarball MS |
|---|---|---|---|
| Kích thước gói Win/Linux | +~55–70 MB cho mọi người (10–12 → ~65–80 MB) [E] | gói CPU giữ nguyên; gói CUDA ~65–80 MB | gói nhỏ; tải thêm GPU lib (vài trăm MB) |
| Chọn thiết bị | trong process, mỗi lần nạp model | người dùng chọn runtime | **một lần mỗi process** (khởi động lại để đổi) |
| Settings | một `settings.json` | tách theo id (`runtime-data/<id>`) → mất API key, default model; phải sửa `settings_store.rs` | một |
| Daemon | không đổi | thêm entry index | không đổi |
| Rủi ro lên đường CPU đã được parity | nhỏ (cu12 `.a` khác `none`, cần chạy lại parity Linux/Windows) | không | lớn (đổi cách link toàn bộ) |
| Blackwell | không (SASS) | không | có (ORT chính thức ≥ 1.21.1 [W]) |
| Công | thấp nhất | +1–1,5 ngày | cao nhất |

**Chọn A** (rồi B nếu không muốn đụng gói CPU). **Libs NVIDIA** không nằm trong gói: `<data_dir>/cuda/` (tải theo yêu cầu, Phase 2) hoặc hệ thống/`SEN_SYSONE_CUDA_DIR` (Phase 1). Linux: preload theo đường dẫn tuyệt đối; hoặc `LD_LIBRARY_PATH={package_dir}/bin` qua `entry.env` (manifest hỗ trợ placeholder trong env). Windows: provider nạp với `LOAD_WITH_ALTERED_SEARCH_PATH` nên DLL cạnh provider tự tìm thấy; libs ở data dir thì preload.

**Dò NVIDIA** — chạy trong runtime, không trong daemon (proxy `/api/decision/*` là generic; thêm field vào `settings_view.defaults` là đủ cho UI):

| Cách | Ưu | Nhược |
|---|---|---|
| `libcuda.so.1` / `nvcuda.dll` qua `libloading`: `cuInit`, `cuDriverGetVersion`, `cuDeviceGetName`, `cuDeviceGetAttribute` (CC), `cuDeviceTotalMem` | trong process, ms, chính xác đúng thứ ORT cần (CUDA tối đa mà driver hỗ trợ + CC) | `cuInit` đánh thức dGPU trên laptop Optimus; thêm dep `libloading` (đã nằm trong cây qua `ort/preload-dylibs`) |
| `nvidia-smi --query-gpu=name,driver_version,compute_cap,memory.total,memory.free --format=csv,noheader,nounits` | không dep, có VRAM trống | spawn 0,1–1 s, có thể không có trên PATH (WSL: `/usr/lib/wsl/lib`), đánh thức GPU |
| NVML (`libnvidia-ml.so.1`/`nvml.dll`) | VRAM trống không cần tạo context, đọc được P-state/clock (hữu ích chẩn đoán idle-clock) | thêm dep |
| File driver: `/proc/driver/nvidia/version`, `/dev/nvidiactl`, `System32\nvcuda.dll` | thụ động, không đánh thức | không có CC/VRAM |

**Khuyến nghị:** kiểm thụ động để gợi ý trên UI; probe chủ động `libcuda` khi nạp (Auto/CUDA) hoặc khi người dùng bấm "Kiểm tra GPU"; **cache theo process**, không probe lúc mở Settings (tránh đánh thức dGPU).

**UI** (Settings → Decision → Cách chạy, cạnh "Số luồng CPU"):

- Select **Thiết bị**: `Tự động` / `CPU` / `CUDA`; mặc định `CPU` ở bản đầu, đổi sang `Tự động` sau khi hardware test đạt.
- `CUDA` vô hiệu hoá + lý do khi `defaults.cuda.available=false` ("không thấy driver NVIDIA", "RTX 5070 (sm_120) chưa được bản build này hỗ trợ", "thiếu libcudnn.so.9"…); kèm tên GPU, driver, CC, VRAM.
- Dòng model đã nạp: `· CUDA (RTX 4070 Ti SUPER)` hoặc `· CPU (lý do)`.
- Như `threads`: model đang nạp giữ thiết bị lúc nạp; gỡ rồi nạp lại để đổi.
- Gợi ý (không tự động hoá): nếu lượt đầu sau khi rảnh chậm → Windows "Prefer maximum performance" cho `sen-sysone.exe`; Linux `sudo nvidia-smi -lgc …` (tốn điện).

### 2.6 CI và kiểm thử

- **Runner GitHub-hosted build được** bản CUDA: `ubuntu-latest`, `windows-latest` đang dùng sẵn; không cần GPU, không cần CUDA toolkit (đã kiểm: `libonnxruntime.a` không tham chiếu cudart/cublas/cudnn; `ort-sys` không phát directive link CUDA). Đặt `ORT_CUDA_VERSION=12`; cache thư mục `ort.pyke.io` (`ORT_CACHE_DIR` đặt được; mặc định `~/.cache/ort.pyke.io` hoặc `%LOCALAPPDATA%\ort.pyke.io`; archive 61/82 MB). Thêm `--features decision-cuda` chỉ cho hai job x64.
- **Test được không GPU:** logic chọn thiết bị (ma trận setting × kết quả probe giả), danh sách preload/tìm lib, định dạng JSON `LoadedInfo`/`defaults.cuda`, và **đường fallback thật**: trên runner (không driver, không libcudart) `device=cuda` phải báo lỗi có chuỗi `libcudart`/`CUDA` — chứng minh provider *được tìm thấy* nhưng thiếu deps — còn `device=auto` phải nạp CPU và trả lời đúng. Cần một fixture ONNX ~2 KB có đủ 5 input + `logits` + `act_logits` (không cần checkpoint 1,29 GB).
- **GPU runner của GitHub:** chỉ tổ chức gói Team/Enterprise Cloud; `linux_4_core_gpu` $0,052/phút, `windows_4_core_gpu` $0,102/phút; docs không nêu model GPU (thường T4, sm_75, nằm trong tập hỗ trợ) [W]. Thay thế: self-hosted trên máy RTX, hoặc thuê GPU cloud theo giờ.
- **Phần cứng tối thiểu trước khi ship:** (1) Linux Ubuntu 24.04 + RTX 30/40; (2) Windows 11 + RTX 20/30/40; (3) một máy GPU không hỗ trợ (Pascal hoặc RTX 50) để thấy Auto hạ cấp. Chi tiết mục 5.

### 2.7 Phương án khác

**DirectML (Windows, mọi GPU DX12).** Binary pyke Windows đã biên dịch sẵn DirectML (cả `none` lẫn `cu12`, kèm `DirectML.dll` 18,5 MB); bật feature `ort/directml` rồi `ep::DirectML::default()`. Không cần thư viện NVIDIA 1,6 GB, chạy cả RTX 50, AMD, Intel. Nhược: ORT ghi DirectML EP "sustained engineering" (tính năng mới chuyển sang WinML), dùng DirectML 1.15.2 và opset ≤ 20 (opset 18 ổn), bắt buộc tắt memory pattern + chạy tuần tự (đã đúng cấu hình hiện tại), và ort-rs ghi "chạy tốt nhất khi kích thước input biết lúc tạo session" → với batch + seq động có nguy cơ biên dịch lại operator mỗi shape. Không tìm thấy benchmark; phải đo trong spike. Cùng áp dụng idle-clock. Nên ship kèm `DirectML.dll` cạnh exe (gói 0.1.2 đang dựa vào System32).

**CoreML (macOS).** EP CoreML đã nằm trong static lib rc.12 cho darwin-arm64 (chuỗi `CoreMLExecutionProvider`, `MLProgram`), nhưng `ort-sys` rc.12 chỉ link `-framework CoreML` cho iOS; macOS cần thêm link arg (rc.13 link cho mọi Apple target và có dist `coreml` riêng). ORT docs: shape động được phép nhưng "performance may be negatively impacted"; không có `ModelCacheDirectory` thì mỗi lần biên dịch lại, "có thể mất vài phút"; danh sách op hỗ trợ hạn chế. Với encoder 22 lớp fp32 1,29 GB, batch + seq động, nhiều khả năng đồ thị bị chẻ nhỏ và phải sao chép trọng số. Chỉ nên làm prototype (MLProgram, `CPUAndGPU`), không hứa trước; chưa chạy thử. Cho người dùng chính, đòn bẩy rẻ hơn có thể là giảm N (vd hỏi `operation` trước rồi chỉ hỏi head của op được chọn, giảm số hàng) — ngoài phạm vi báo cáo này.

Ghi chú một dòng: pyke còn có build `wgpu` (Vulkan/D3D12/Metal) cho cả ba nền tảng, ort-rs ghi WebGPU EP "experimental, có thể sai kết quả/crash".

## 3. Thay đổi cần làm theo repo (Phase 1, nếu qua cổng)

### `sen-sysone` (`/Users/benji/Projects/SenClaw/sen-sysone`)

| File | Thay đổi |
|---|---|
| `Cargo.toml` | feature `decision-cuda = ["decision-laya", "ort/cuda", "ort/preload-dylibs", "dep:libloading"]`, thêm `libloading` optional; giữ `=2.0.0-rc.12`; cập nhật comment pin |
| `src/decision/laya/engine.rs` | `build_session(layout, threads, device)`: đổi sang `ort::ep::CPU`; nhánh CUDA như 2.1; probe run sau `commit_from_file`; dựng lại CPU khi lỗi (Auto); luồng suy luận CUDA riêng; hạ cấp xuống CPU khi `run` CUDA lỗi |
| `src/decision/laya/device.rs` (mới, `cfg(feature = "decision-cuda")`, kèm stub) | `probe_cuda()` (libloading: cuInit/cuDriverGetVersion/tên/CC/VRAM, cache theo process); bảng CC hỗ trợ của build; `locate_nvidia_libs()` (`SEN_SYSONE_CUDA_DIR` → `<data_dir>/cuda` → `CUDA_PATH`/`CUDA_HOME` → loader mặc định; duyệt cả bố cục pip `nvidia/*/lib`); `preload()` tự viết |
| `src/decision/laya/runtime.rs` | `LoadedInfo` thêm `device`, `device_name`, `device_note`; `load`/`spawn_load` nhận thiết bị; log + `info_detail` |
| `src/decision/settings.rs` | `LocalSettings.device` (`auto` / `cpu` / `cuda`, `serde(default)` → tương thích ngược), `validated()` |
| `src/http.rs` | `decision_model_load` truyền thiết bị; `settings_view().defaults.cuda = {compiled, available, name, driver, computeCap, vramMb, libs, note}` |
| `scripts/bundle-shared-libs.sh`, `Makefile`, `.github/workflows/release.yml` | build `--features decision-cuda` + `ORT_CUDA_VERSION=12` cho linux-x64/windows-x64; `cp -L` `libonnxruntime_providers_{cuda,shared}.so` / `.dll` (+ `DirectML.dll` trên Windows) cạnh exe; bỏ các provider TensorRT/NVRTX; **không** patch lib NVIDIA |
| `senclaw-runtime.json` | chỉ tăng version (khớp `tests/manifest.rs`) |
| `tests/` + `src/decision/laya/` | fixture ONNX nhỏ, test logic thiết bị, test fallback; chạy lại `parity_tests` với `device=cpu` và (thủ công) `cuda` |
| `docs/laya-decisions.md` | mục "Chạy trên GPU NVIDIA" |

### `senclaw` daemon (`/Users/benji/Projects/SenClaw/senclaw`)

| File | Thay đổi |
|---|---|
| `docs/runtime-protocol.md` §4.3 | ghi `settings.local.device`, `defaults.cuda`, `loaded.device*` |
| `src/runtime/*` | **Không đổi** (proxy generic; daemon không cần dò GPU) |
| `runtimes/index.json` | **không sửa lúc này** (session khác đang có thay đổi chưa commit). Khi phát hành: thêm `releases[]` mới; nếu chọn phương án B thì thêm entry `sen-sysone-cuda` (`accelerator: "cuda"`) |
| `src/browser_agent/*` (tuỳ chọn) | `warm_decision_models` đã nạp khi mở trang; chỉ thêm gọi `POST /api/decision/warm` nếu spike chứng minh heartbeat có hiệu quả |

### `web-app` và `desktop`

| File | Thay đổi |
|---|---|
| `web-app/src/components/settings/decisionApi.ts` | `LoadedInfo` (dòng 31), `RunSettings.local` (dòng ~67/84), `SettingsView.defaults.cuda` |
| `web-app/src/components/settings/DecisionRunSettings.tsx` | Field "Thiết bị" cạnh "Số luồng CPU" (dòng ~201); chuỗi mới qua `t()` + `src/i18n/vi.web.json` (quy tắc trong `web-app/CLAUDE.md`) |
| `web-app/src/components/settings/DecisionSettings.tsx` | dòng ~412 thêm thiết bị vào chú thích model đã nạp |
| `desktop/lib/features/settings/decision_models.dart` | `LayaLoaded` (dòng 47), `DecisionLocalSettings` (dòng 193): `device` |
| `desktop/lib/features/settings/decision_run_settings.dart` | dropdown cạnh "CPU threads" (dòng ~202) |
| `desktop/lib/features/settings/decision_section.dart` | hiện thiết bị ở dòng model đã nạp |

## 4. Rủi ro

| Rủi ro | Mức | Giảm thiểu |
|---|---|---|
| Lợi ích mỗi bước không như kỳ vọng (idle-clock; fixture nhỏ so với trang thật) | Cao | Spike + cổng; đo N thật; chỉ opt-in |
| Binary pyke thiếu SASS cho Pascal/Volta/**Blackwell** → lỗi ở lần `run` đầu | Cao | Bảng CC trong probe; probe run bắt buộc; thông báo rõ; chờ crate mới hoặc phương án C |
| VRAM ~3–4 GB tranh với `llama.cpp-cuda` | Cao với người dùng LLM local | Auto yêu cầu VRAM trống ≥ ~4 GB; `gpu_mem_limit`; thứ tự nạp; sau này fp16 |
| Libs NVIDIA 1,6 GB (cu12) và điều khoản giấy phép | Trung–Cao | Không bundle; tải từ NVIDIA khi người dùng đồng ý; NOTICE; xem xét pháp lý; dài hạn rc.13 (~0,5 GB) |
| Fallback im lặng của ort bị hiểu nhầm; lỗi nằm ở 3 tầng | Trung | `error_on_failure` + probe run + hạ cấp; test fallback trong CI |
| `ort` là pre-release (rc), một maintainer, pipeline binary đổi hàng tuần (rc.13 bỏ CUDA 12; sm_120 thêm 2026-09-23) | Trung | Giữ pin + sha256 trong crate; feature gate; theo dõi `plugin-ep-cuda` của ORT |
| glibc ≥ 2.38 (provider và exe hiện tại) → loại Ubuntu 22.04/Debian 12 | Trung | Ghi rõ trong UI/doc; xét build trên môi trường cũ hơn nếu pyke lib cho phép (lib prebuilt cũng cần 2.38) |
| Parity: TF32 mặc định bật; kernel CUDA khác CPU; build `cu12` khác `none` | Trung | `tf32=false`; chạy parity CPU+CUDA; dung sai 1e-3 giữ nguyên |
| Churn context theo thread; context CUDA (~0,3–0,5 GB) còn sau unload | Thấp–Trung | Thread riêng; idle-stop của daemon (300 s) giải phóng tiến trình |
| dGPU laptop bị đánh thức; tốn pin | Thấp–Trung | Probe lười, cache; không heartbeat mặc định |
| Gói Windows hiện không ship `DirectML.dll` (phụ thuộc System32) | Thấp | Ship cạnh exe |

## 5. Kế hoạch kiểm thử

| Cấp | Nội dung | GPU? | Ở đâu |
|---|---|---|---|
| Spike (Phase 0) | Bật `controlPlane.recordDecisionInputs` 1 ngày để lấy request thật + N thật; phát lại trên máy RTX: nhịp liên tục 50 request, nghỉ 2 s ×50, nghỉ 5 s ×50, heartbeat các duty cycle; fp32 / TF32; `nvidia-smi dmon -s pc -d 1` ghi clock; Windows thêm DirectML; `with_disable_cpu_fallback` xem có node rơi CPU | Có | Máy mượn / cloud RTX |
| Unit | Chọn thiết bị (setting × probe), preload list, tìm lib, JSON UI | Không | `cargo test`, CI mọi OS |
| CI tích hợp | Fixture ONNX nhỏ: `auto` → CPU đúng; `cuda` → lỗi có `libcudart`/`CUDA`, không crash; gói có provider cạnh exe; exe không `NEEDED` CUDA (`ldd`/`dumpbin`) | Không | `ubuntu-latest`, `windows-latest` |
| Parity CPU | `parity_tests` trên build cu12 Linux/Windows (cần checkpoint) | Không | Thủ công / self-hosted |
| Phần cứng tối thiểu | Load `multilingual`; parity ≤ 1e-3 (`tf32=false`); latency nóng/lạnh; kéo `libcudnn`, `CUDA_VISIBLE_DEVICES=-1`, driver cũ → Auto hạ cấp đúng | Có | Linux 24.04 + RTX; Windows 11 + RTX |
| GPU không hỗ trợ | Pascal hoặc RTX 50: Auto hạ cấp có note; `CUDA` hiện lỗi 209 | Có | 1 máy |
| Soak | 200 quyết định cách 2 s; VRAM ổn định; unload/nạp lại 20×; gate + browser đồng thời | Có | Máy RTX |

## 6. Ước lượng công sức [E]

Một kỹ sư đã quen các repo; ngày công.

| Phase | Việc | Ngày |
|---|---|---|
| 0 | Spike + cổng (kèm thuê GPU vài giờ, ~US$1–5) | 0,5–1 |
| 1 | `sen-sysone`: feature, `device.rs`, engine 3 tầng + probe + thread riêng, settings, `LoadedInfo`, http | 4–5 |
| 1 | Đóng gói (Makefile, bundle script, release.yml, cache) | 1 |
| 1 | Test (fixture, unit, CI fallback, parity chạy lại) | 1,5 |
| 1 | `web-app` + `desktop` (kèm i18n) | 2–3 |
| 1 | Docs (`laya-decisions.md`, `runtime-protocol.md`) | 0,5 |
| 1 | Hardware validation (Win + Linux + GPU không hỗ trợ) | 1,5–2 |
| | **Tổng Phase 1** | **~10–13** |
| 2 | Trình cài libs NVIDIA (tìm wheel/redist, sha256, resume, giải nén, tiến độ, UI) | 4–6 |
| 2 | Nâng rc.13 + parity (macOS/Win/Linux) — làm riêng | 2–3 |
| 2 | `POST /api/decision/warm` + gọi từ vòng lặp (chỉ khi spike ủng hộ) | 0,5–1 |
| B | Thay A bằng id riêng `sen-sysone-cuda` (index, CI matrix, sửa đường dẫn settings) | +1–1,5 |

## 7. Nguồn (phiên bản / ngày)

**[S] Source crate và binary** (thư mục `~/.cargo/registry/src/index.crates.io-*`; archive tải vào scratch, đã xoá)

- `ort 2.0.0-rc.12` (crates.io 2026-03-05, ORT 1.24): `Cargo.toml`, `src/ep/{mod,cuda,cpu,directml,coreml}.rs`, `src/util/mod.rs`, `src/device.rs`, `src/lib.rs`. `ort-sys 2.0.0-rc.12`: `build/main.rs`, `build/vars.rs`, `build/dynamic_link.rs`, `build/static_link/mod.rs`, `build/download/{dist.txt,resolve.rs,mod.rs}`.
- `ort`/`ort-sys 2.0.0-rc.13` (2026-07-28, ORT 1.28): `build/download/{dist.tsv,resolve.rs}`, `build/static_link/mod.rs`, `src/ep/{mod,cuda}.rs`, `src/lib.rs`.
- Archive `cdn.pyke.io` ORT 1.24.2 (build 2026-02-20): linux-x64 `none`/`cu12`/`cu13`, windows-x64 `none`/`cu12` (sha256 khớp `dist.txt`); ORT 1.28.0 `x86_64-unknown-linux-gnu+cuda13,tensorrt,nvrtx` (2026-07-25, sha256 khớp `dist.tsv`). Đã xem: danh sách file, ELF `DT_NEEDED`/`RUNPATH`, PE imports, header fatbin (kiến trúc GPU), quét chuỗi `libonnxruntime.a`. Chỉ kiểm provider Linux; Windows giả định cùng pipeline.
- Gói `sen-sysone 0.1.2` windows-x64 và linux-x64 (GitHub release, URL trong `runtimes/index.json`): PE imports, ELF `NEEDED`, ký hiệu glibc.
- ORT `v1.24.2` GitHub raw: `core/platform/{posix,windows}/env.cc`, `core/session/provider_bridge_ort.cc`, `core/providers/cuda/{cuda_execution_provider.cc,cuda_execution_provider_info.h,cuda_provider_factory.cc}`.
- `pykeio/ort-artifacts` `main` @ 2026-10-01: `src/build.ts` (`CMAKE_CUDA_ARCHITECTURES=75;80;90;120`, CUDA 13.2, cuDNN 9.23.2), lịch sử commit (`61e5324d`, 2026-09-23, "Add sm_120 …").
- SenClaw: `sen-sysone/{Cargo.toml,Makefile,scripts/bundle-shared-libs.sh,.github/workflows/release.yml,senclaw-runtime.json,CLAUDE.md,src/decision/laya/{engine,runtime}.rs,src/decision/settings.rs,src/http.rs,src/settings_store.rs}`; `senclaw/{docs/runtime-protocol.md,src/runtime/{llamacpp,index,settings,supervisor}.rs,crates/sen-runtime-sdk,runtimes/index.json (chỉ đọc),src/browser_agent/encoder.rs,CLAUDE.md}`; `web-app/src/components/settings/*`; `desktop/lib/features/settings/*`; báo cáo `research-260930-2059-jev-ultrafast-action-latency.md`.

**[W] Tài liệu web** (truy cập 2026-10-01)

- ONNX Runtime CUDA EP, DirectML EP, CoreML EP (onnxruntime.ai; bảng phiên bản tới ORT 1.30.x); ORT GitHub releases: v1.21.1 (2025-04-21), v1.24.1 (2026-02-06), v1.27.0 (2026-06-19), v1.28.0 (2026-07-25), v1.30.0 (2026-09-10).
- `ort.pyke.io/setup/linking` (footer 2026-03-06), `/perf/execution-providers` (footer 2026-07-28); crates.io API (`ort` max `2.0.0-rc.13`).
- NVIDIA: CUDA Toolkit 13.4 Update 1 release notes (Table 3/4 driver tối thiểu; cuFFT: bỏ Maxwell/Pascal/Volta); CUDA Toolkit EULA (Attachment A); cuDNN SLA; redist manifests CUDA 12.9.2 / 13.4.2, cuDNN 9.27.0; PyPI `nvidia-*-cu12` (CUDA 12.9.x, cuDNN 9.27.0.42), `nvidia-cublas 13.8.1.7`, `nvidia-cuda-runtime 13.4.92`, `nvidia-curand 10.4.4.72`; TensorRT 10.x "GPU Clock Locking and Floating Clock".
- HF model card `cklxx/laya-browser` README @ `645cf366` (2026-09-29); HF `jhu-clsp/mmBERT-base/config.json`; ORT `v1.24.2` `python/onnxruntime_pybind_state.cc`; ModernBERT arXiv 2412.13663 v2 Table 2; PyTorch forums thread 224902 (first batch after idle); MS Open Source blog 2020-01-21; NVIDIA dev forum (TensorRT BERT).
- GitHub docs: larger runners, Actions runner pricing; llama.cpp release `b11319` (2026-10-01); trang spec RTX 4070 Ti SUPER (thứ cấp).

**[E] Ước lượng của tôi**: toàn bộ công thức/cột "ước lượng" ở 2.4, kích thước gói +55–70 MB, VRAM 3–4 GB, tập con cuDNN ~320 MB, ngưỡng cổng, ngày công ở mục 6, tỉ lệ đóng góp ~0–25% vào thời gian task.

## 8. Giới hạn nghiên cứu

- Không có GPU: chưa đo độ trễ ORT CUDA, idle-clock, VRAM, node placement, hay hành vi lỗi thực tế; mô tả fallback là suy ra từ source ORT 1.24.2.
- Chưa kiểm provider Windows (fatbin, kiến trúc), binary rc.13 Windows, và bản pyke cu12 build với CUDA minor nào.
- Không chạy thử DirectML, CoreML, WebGPU, TensorRT, fp16 export; các nhận định là từ docs/source.
- Không tư vấn pháp lý; phần giấy phép chỉ trích điều khoản công khai.
- Chưa đánh giá ORT CUDA *plugin EP* (`plugin-ep-cuda v0.1.0`, 2026-08-17) — có thể thay cơ chế provider-bridge.
- N thật của vòng lặp browser chưa biết; mọi suy luận lợi ích phụ thuộc N.

## 9. Câu hỏi chưa giải quyết

1. Phân bố N (token/request) thật của bước browser và gate trên trang thật? (đọc `usage.input_tokens` hoặc `recordDecisionInputs`)
2. Heartbeat/keep-alive giữ được clock ở duty cycle nào, tốn bao nhiêu W trên GPU consumer? Có đáng làm không?
3. ORT CUDA EP chạy đồ thị mmBERT opset 18 thật: latency nóng/lạnh fp32, có node nào rơi về CPU / sinh Memcpy không?
4. Có bao nhiêu người dùng Windows/Linux + NVIDIA, và tỉ lệ RTX 50? (quyết định có chờ crate mới / phương án C hay không)
5. DirectML với batch + seq động có dùng được không (biên dịch lại mỗi shape)? Có thể làm CUDA thừa trên Windows?
6. Tập con cuDNN thật sự cần cho Laya trên ORT 1.24.2 (giảm 1,2 GB → ~0,3 GB)? Có thể bỏ cuFFT/cuRAND không (hiện là `NEEDED` cứng)?
7. Bundle hay tải libs NVIDIA: ý kiến pháp lý; có tái dùng `cudart-llama-*` của llama.cpp được không (thiếu cuFFT/cuRAND/cuDNN)?
8. Nâng rc.13/ORT 1.28: chênh lệch parity/hiệu năng CPU trên macOS? Bao giờ pyke phát hành crate chứa binary có sm_120?
9. Tổ chức GitHub `SenClaw` có gói Team (để dùng GPU runner) không? Model GPU của runner là gì?
10. Cùng GPU, `llama.cpp-cuda` và Laya chia VRAM thế nào trong thực tế; Auto có nên nhường GPU cho LLM?
11. Linux: có cần hỗ trợ glibc < 2.38 (Ubuntu 22.04)? Hiện cả gói CPU lẫn provider đều không chạy.
12. Có nên mở rộng cùng thiết kế cho `sen-tts` (cũng dùng `ort =rc.12`)?
