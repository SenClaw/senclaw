# Kiểm chứng P5 (daemon): đo trên model thật và tài liệu

Ngày 2026-10-19 · Nền `e8b5264` (`feat/project`, worktree `.worktrees/senclaw-project`) · **Chưa commit** (theo ràng buộc)
**Cập nhật 02:30: phần A đã đo trên gemma-4-e2b (qua sen-mlx) — KHÔNG phải e4b của người dùng; xem "Bổ sung" từ mục 7. Các mục 0–6 là bản viết lúc LM Studio tắt; chỗ nào mâu thuẫn với mục 8–11 thì mục 8–11 đúng.** Kết quả ngắn: ngưỡng của #3 bảo TẮT ⇒ mặc định đã đổi sang TẮT; #2 giữ.
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P5, quyết định #2 và #3 · [design.md](../261018-1618-project-feature/design.md) R4, R9, §10 · [P3b](test-261019-0035-project-p3b.md)
Công cụ đo + harness: [project-p5-measure/](project-p5-measure/) · test: `src/memory/engine/eval/real_project.rs`, `evals/memory/project-measure.json`

## 0. KẾT QUẢ CHÍNH (bản lúc LM Studio tắt — số đo trên e2b ở mục 8)

**Phần A (đo trên model thật) KHÔNG chạy được: chưa có một con số nào về gemma-4-e4b.** Máy chủ LM Studio của người dùng (`http://192.168.0.93:1234/v1`, model `google/gemma-4-e4b`, key rỗng — đọc từ `~/.senclaw/config.json`, chỉ đọc)
không trả lời `GET /v1/models`:

| Kiểm | Kết quả (2026-10-19 ~01:00–01:12, lặp nhiều lần) |
|---|---|
| `curl -m 5..8 http://192.168.0.93:1234/v1/models` | lặp ≥ 10 lần trong ~20 phút: lần đầu sau một quãng nghỉ timeout (`curl: (28)`), các lần ngay sau đó thất bại tức thì (`Couldn't connect`, 1 ms — hàng xóm ARP đã bị đánh dấu không tới được) |
| `ping 192.168.0.93` | 100% mất gói; `nc -z` cổng 1234 không thông |
| Máy này | `en0` 192.168.0.82/24 (cùng mạng con); ping gateway `192.168.0.1` bình thường (3 ms); `arp` còn bản ghi cho 192.168.0.93 (cũ) |
| LM Studio cục bộ | app có cài, nhưng không có model gemma 4 trong `~/.lmstudio/models` (chỉ Qwen3-4B-MLX, Qwen2.5-VL-3B); cổng 1234 cục bộ không nghe |

Theo yêu cầu ("không tới được thì dừng phần A, ghi rõ là chưa đo, đừng giả") tôi **dừng phần A** và không thay bằng model khác. Cũng không dùng cấu hình DeepSeek/Gemini của người dùng (đó là khoá và tiền của họ, không phải model được hỏi).
Hệ quả: **không có số liệu cho (1), (2), (3) của phần A; không có căn cứ đo để đổi mặc định #2 hay #3** (xem mục 4).

Cái đã làm thay: dựng đủ công cụ để khi máy chủ bật lên thì đo bằng **ba lệnh** (mục 3), và chạy thử toàn bộ bằng một mock **chỉ để kiểm đường ống** (mục 2). Mock không hiểu gì; số của nó không phải số đo và không được trích ở đâu.

**Phần B (tài liệu) xong.** `cargo check --all-targets` sạch (2 cảnh báo có sẵn), `cargo test --lib` **3255 passed, 0 failed, 13 ignored** (11 cũ + 2 test đo mới), kể cả `session_store::tests::round_trip_save_load_clear` (xanh lần này).

## 1. Theo phần việc

| Việc giao | Trạng thái | Ghi chú |
|---|---|---|
| A. kiểm máy chủ LM Studio | **không tới được** (mục 0) | không đo gì |
| A(1) R4 / #2: fact cá nhân vào bank project; recall chat ngoài project mất bao nhiêu | **đo trên e2b** (mục 8.1); e4b chưa | `real_extractor_personal_facts_in_a_project_chat` ×3 |
| A(2) #3: ≥ 20 summary qua `retain`: số fact, hữu ích / rác / trùng | **đo trên e2b** (mục 8.2); e4b chưa | `real_extractor_card_summaries` (28 summary) ×3 ⇒ ngưỡng bảo TẮT (mục 9) |
| A(3) worker thật: ≥ 6 card trên board gắn project | **đo trên e2b** (mục 8.3); e4b chưa | `exp-p5-worker.mjs` (8 card) + proxy ghi log |
| B(1) `docs/projects.md` | **xong** (181 dòng) | mục 1–10 theo yêu cầu, gồm bảng REST/WS, 14 hạn chế & bẫy, mục "Chưa đo" |
| B(2) `docs/kanban-guide.md` | **xong** | board gắn project, thư mục hiệu lực, worker nhận gì, card xong lưu fact, REST, 3 bẫy mới (lỗi finalize-block, hai lỗi board AI) |
| B(2) `docs/memory-engine.md` | **xong** | hai hàng mới trong bảng "ai đọc/ghi" (kho fact project, fact từ card kanban), mục "Project: bank thứ hai", hai test đo mới |
| B(2) `CLAUDE.md` mục "Projects" | **xong** (+1 dòng ở "Key layers") | 8 luật mới; không lặp các gạch đầu dòng P2/P3b đã có |
| Báo cáo + `plan.md` | **xong** | file này; dòng P5 đã cập nhật |

## 2. Cái đã làm cho phần A (chưa có số đo)

1. **`src/memory/engine/eval/real_project.rs`** (mô-đun con của `eval.rs`, thêm đúng một dòng `mod real_project;`) + **`evals/memory/project-measure.json`**. Hai test `#[ignore]`, dùng client production `OpenAiCompatLlm` (timeout 60 s tính vào số đo) và pipeline `retain` thật; từ chối cổng 18788/18789 như `real_extractor_*`.
   - `real_extractor_personal_facts_in_a_project_chat`: **12 cửa sổ** hội thoại của một chat project (5 về dự án "Sao Kim", 4 cá nhân — tên, nơi ở, cà phê, mèo, chạy bộ —, 3 lẫn cả hai). In từng fact được lưu vào bank project và nhãn theo marker (`project`/`personal`/`both`/`other`). Rồi hỏi **7 câu cá nhân** từ: chính chat đó, một chat ngoài project (fact theo chat — mặc định), một chat ngoài project (`SENCLAW_MEMORY_FACTS_SCOPE=agent`), một chat khác của project; so với **cùng câu trả lời của model** được lưu theo kiểu trước khi có project (bank profile gắn chat) — nên khác biệt chỉ do nơi lưu, không do model trả lời khác. Thêm **5 câu dự án** (kể cả ca đổi cổng 8420→8431 để xem `replaces`) hỏi từ chat khác của project và đo rò sang chat ngoài.
   - `real_extractor_card_summaries`: **28 summary** kiểu worker viết, qua `retain` đúng văn bản dispatcher dùng (`Kanban task "<tiêu đề>" completed: <summary>`, `source=kanban`): 4 tốt, 2 có số liệu dày, 1 đường dẫn, 5 chung chung ("Done.", "Task completed successfully."…), 4 ngắn, 2 dài (≈ 700–900 ký tự, lẫn nhiễu), 3 tiếng Việt (dài, ngắn, "Xong rồi."), 1 câu hỏi của worker, 1 kết quả âm, 3 về cùng một giá trị (20→50, lặp lại, →25), 1 có secret, 1 về đồng nghiệp In fact sinh ra của từng card (`useful` nếu chứa marker của card, `JUNK?` nếu không có marker hoặc card vốn "không có gì đáng lưu"), bank cuối, và **7 câu hỏi** "worker kế tiếp được nghe gì" (có ca giá trị cũ không được phép quay lại).
   - Cả hai chỉ **in**, không chấm đúng/sai; người chạy đọc output (như `real_extractor_*`). Dry-run bằng mock: biên dịch, chạy hết 12 cửa sổ / 28 summary / 12+7 câu hỏi, in đúng định dạng. Số của mock vô nghĩa.
2. **`plans/reports/project-p5-measure/`**: `llm-proxy.mjs` (proxy ghi log đứng trước model: từng lời gọi của worker và extractor, công cụ nào được mời, tool call nào model gọi, độ trễ, thân yêu cầu), `env-up.sh` / `env-down.sh` (daemon cô lập — `HOME`, `SENCLAW_HOME`, cwd, cổng 53788/53789/53931, Seatbelt chặn kết nối loopback tới 18788/18789/18790, `SENCLAW_BASE_URL` = cổng UI của chính nó, `SENCLAW_APP_TOKEN_MODE=off`, **từ chối khởi động nếu `GET /models` của model không trả lời**), `exp-p5-worker.mjs` (project + board + **8 card chạy tuần tự**: tạo file, đọc file, đếm dòng/tổng cột CSV, viết hàm, tìm hàm, trả lời từ context không mở file, card phải bị block, hỏi cổng — để card sau chỉ có thể nghe fact do card trước để lại), `mock-extractor.mjs` (chỉ để dry-run).
   - Driver đếm: card kết thúc bằng `kanban_complete` + summary / `kanban_complete` summary rỗng / `kanban_block` / dispatcher tự đóng / chưa xong; việc có làm đúng không (kiểm file hoặc nội dung summary); số yêu cầu và tool call của worker; card có nhận khối `<memory_context>` không và có chứa fact của card trước không; extractor có được hỏi không; số fact thêm vào bank. Xuất `result-p5-worker-<label>.json` + `proxy-requests-<label>.jsonl`.
   - **Dry-run** (daemon thật của nhánh `e8b5264`, binary `p4-daemon/senclaw` sha256 `00f50dca…`, model = mock không gọi tool): chạy hết 8 card trong ~80 giây; cả 8 do dispatcher tự đóng ("worker called neither"), 0 retain, 0 fact, proxy ghi 8 yêu cầu worker (33 tool được mời, card id đọc đúng). Chứng minh driver, proxy và đường "dispatcher tự đóng" chạy; **không** là số đo. Mọi tiến trình dừng (kiểm cổng 53788/53789/53931/59931 trống); daemon thật 18788/18789 và `~/.senclaw` không bị đụng; bằng chứng dry-run đã xoá khỏi repo.
3. Sai khác đã biết giữa daemon thử và cài đặt thật khi đo: thử **không có embedding model** (cài thật dùng `all-MiniLM-L6-v2` cục bộ) ⇒ recall là từ khoá/thực thể/ngày như cài đặt mặc định; `thinkingEnabled=true` được giữ như cài thật.

## 3. Chạy lại khi máy LM Studio lên (≈ 20–40 phút, chưa tính thời gian model)

```bash
# 0. model có trả lời không? (nếu không: dừng, đừng đo)
curl -s -m 8 http://192.168.0.93:1234/v1/models
# 1+2. hai test ignore, chạy 3 lần mỗi test (temperature 0.1 nhưng LM Studio vẫn lệch giữa các lần), HOME/cwd tạm
S=<scratch>; mkdir -p $S/home $S/cwd; cd $S/cwd
env -u SENCLAW_HOME HOME=$S/home SENCLAW_HOME=$S/home/.senclaw \
  SENCLAW_EVAL_LLM_BASE_URL=http://192.168.0.93:1234/v1 SENCLAW_EVAL_LLM_MODEL=google/gemma-4-e4b \
  <test binary> memory::engine::eval::real_project --ignored --nocapture --test-threads=1 | tee $S/p5-run1.log
# 3. worker thật (binary daemon feat/project @ e8b5264; cổng rảnh, KHÔNG 18788/18789/18790)
export P0_RUNS=$S/runs P0_UI=53788 P0_WS=53789 P0_MOCK=53931
cd senclaw/plans/reports/project-p5-measure
P5_LLM_BASE=http://192.168.0.93:1234/v1 P5_LLM_MODEL=google/gemma-4-e4b ./env-up.sh p5 <senclaw binary>
P5_LLM_MODEL=google/gemma-4-e4b node exp-p5-worker.mjs p5 ; ./env-down.sh p5
```

Binary test lib: `cargo test --lib --no-run` (dùng chung `CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/senclaw/target`, `CARGO_INCREMENTAL=0`), **sao ra scratch ngay** (hash trùng giữa các worktree). Dòng `summary […]` của mỗi test gom số; đọc cả phần in từng fact vì nhãn `JUNK?`/`personal` là theo marker, không phải phán quyết.

**Ngưỡng nên chốt TRƯỚC khi nhìn số** (đề xuất của tôi, người dùng đổi được):
- **#3** đổi mặc định sang **TẮT** nếu trên `real_extractor_card_summaries` (a) bất kỳ summary rỗng/chung chung nào sinh ra fact (đếm `N/8 empty summaries left no fact` < 8/8 ở ≥ 2/3 lần chạy), hoặc (b) `junk candidates` ≥ 30% số fact đã lưu sau khi đã đọc tay và loại các ca nhãn sai, hoặc (c) fact có secret/`sk_live`; hoặc nếu ở worker thật phần lớn card không bao giờ gọi `kanban_complete` kèm summary có nghĩa (khi đó tính năng gần như không hoạt động — không rác nhưng cũng không có lợi, nên giữ BẬT vô hại).
- **#2** (fact cá nhân ở lại bank project) xem lại nếu ≥ 25% fact trong bank project là `personal`/`both` **và** có cách dùng thật mà người dùng cần chúng ở chat ngoài project; ở scope mặc định (theo chat) bảng "câu cá nhân hỏi từ chat ngoài project" trước/sau sẽ giống nhau ở cột scope-theo-chat (fact đó vốn không thấy ở chat khác), nên mất mát thật chỉ có với `SENCLAW_MEMORY_FACTS_SCOPE=agent`.

## 4. Khuyến nghị cho quyết định #2 và #3 (bản lúc chưa có số đo — thay bằng mục 9)

- **Không đủ căn cứ để đổi mặc định.** Cả hai vẫn là mặc định của thiết kế, chưa phải kết luận từ số đo; tài liệu (`projects.md` mục 9, `kanban-guide.md`) nói thẳng như vậy.
- **#2**: giữ ("ở lại bank project"). Lý do vẫn đứng được không cần số đo: ở scope mặc định (theo chat) việc fact cá nhân nằm trong bank project không làm mất gì mà chat ngoài vốn thấy; danh tính thật nằm ở `USER.md`.
- **#3**: nếu phải **phát hành trước khi đo** được, tôi nghiêng về **TẮT** (đây là phán đoán của tôi, không phải số đo; quyết định thuộc người dùng). Lý do: đây là đường ghi tự động duy nhất vào bank project không qua người gõ (summary có thể chứa nội dung web worker vừa đọc — P3b §6.3), model nhỏ chưa được đo là có tách được tóm tắt hữu ích khỏi "Done."/câu hỏi/nhiễu hay không, rác tích luỹ lặng lẽ và chỉ xoá tay từng fact, trong khi bật lại sau khi đã đo thì rẻ. Ngược lại nếu chờ đo được thì làm theo ngưỡng ở mục 3. Công tắc đã có: `facts_write=chat|off` hoặc một dòng đổi mặc định ở `worker_fact_target`.
- Hai điều P3b đã nêu và vẫn nên nhìn khi đo: `finalize` đẩy card đã `kanban_block` sang Done (§6.1 P3b — tài liệu đã ghi); fact gõ tay thắng và recall gộp gần-trùng nên "cổng đã đổi 8123" từ card có thể bị che bởi "cổng 7421" gõ tay (`card_questions[page-size]` và card W8 trong driver đo đúng chỗ này).

## 5. Phần B — đã viết gì, đã đối chiếu với đâu

| File | Thay đổi | Đối chiếu |
|---|---|---|
| `docs/projects.md` (mới, 181 dòng) | project là gì + trường; **luật root** với bảng mã lý do (đọc từ `projects/root.rs`); session (gán lúc tạo, thư mục, state file, DAG, env); context (8.000 ký tự, 100 KB, 409/413, vị trí trong prompt); facts ba mức + `facts_write` + nhập tay/xoá; lưu trữ / purge; kanban; bảng REST/WS + mã lỗi; **14 hạn chế & bẫy**; mục "Chưa đo" | `ui_server/projects.rs`, `projects/{root,context,session,facts}.rs`, `websocket_gateway/handlers.rs`, các báo cáo P0–P3b |
| `docs/kanban-guide.md` | khái niệm + "Board của một project" + "Worker của board gắn project nhận gì" + REST + bẫy 9–11 (lỗi finalize-block; hai lỗi board AI và nhánh `fix/kanban-llm-loopback` chưa gộp; trỏ sang hạn chế project) | P3a §6, P3b §6.1, `kanban/api.rs` |
| `docs/memory-engine.md` | nguồn `kanban`; hai hàng bảng "ai đọc/ghi"; mục "Project: bank thứ hai" (đích ghi, đọc, worker, `manual`, xoá theo lô, cái đã/chưa đo); dòng bank project trong "Xem và xoá"; hai test `real_project` trong mục eval model thật | P2, P3b |
| `CLAUDE.md` | mục "Projects" (8 luật: project cố định lúc tạo session; `upsert_group` COALESCE; `workspace_state_key` một chỗ; env lúc spawn + parent xếp hàng; root kiểm lại khi dùng; context đọc mỗi lượt/không tool ghi/vị trí `<project>`; kanban qua `ProjectDirectory` + guidance chứ không `custom_rules`; lưu trữ vs purge; giới hạn đã chấp nhận) + một dòng ở "Key layers" | P0–P3b; tránh lặp bullet P2/P3b |

Lưu ý khi gộp: link trong `projects.md` tới `../plans/261018-1618-project-feature/` và `plans/reports/*` trỏ vào thư mục `plans/` của checkout chính (chưa nằm trong worktree/nhánh `feat/project`) — chúng chỉ hết chết khi plan và báo cáo được commit cùng.

## 6. An toàn môi trường

Không chạm daemon thật 18788/18789 hay `~/.senclaw` (chỉ **đọc** `config.json` để lấy `baseURL`/`modelName`; `apiKey` không in, chỉ biết "rỗng"). Mọi tiến trình do tôi chạy (mock 59931, proxy 53931, daemon 53788/53789) đã dừng, cổng trống. `target/debug/senclaw` (daemon thật) không bị ghi đè: test build vào `target/debug/deps` dùng chung, binary test sao ra scratch ngay; không có build nào chạy cùng lúc (`ps` kiểm trước khi `cargo`). Không `cargo fmt` toàn repo: chỉ `rustfmt` file mới `real_project.rs`. Không commit.

## Chưa kiểm chứng (tổng)

- Mọi thứ của phần A (mục 0): chất lượng trích xuất, R4, độ nhiễu fact từ summary card, worker thật gọi `kanban_complete` kèm summary có nghĩa hay không, recall có đưa fact card trước vào card sau với model thật hay không.
- Driver `exp-p5-worker.mjs` và hai test mới chưa từng chạy với model thật: có thể cần chỉnh khi gặp hành vi thực (ví dụ e4b "nghĩ" ~1.000 token trước khi trả lời, `ToolSearch` bắt buộc trước tool MCP kanban, timeout 60 s của extractor).
- Docs mô tả REST/WS; giao diện web/desktop (P4) chưa có trong đó. Docs chưa được đọc lại bởi người khác ngoài đối chiếu với mã.
- Chỉ macOS.

## Câu hỏi chưa giải quyết (bản đầu; bản cập nhật ở cuối file)

1. Bật máy `192.168.0.93` (LM Studio, `google/gemma-4-e4b`) để chạy mục 3? Hoặc cho phép đo trên **model thay thế cục bộ**? **Đính chính (2026-10-19 01:40): bản đầu của câu này nói `mlx-community/gemma-4-e4b-it-4bit` "đã có trong cache HF" — SAI.** Thư mục cache chỉ là vỏ rỗng (4 KB, không có weights, không có `snapshots/`); không có weights e4b nào trên máy này. Cái có sẵn trên đĩa: `~/.senclaw/local-models/mlx-community__gemma-4-e2b-it-OptiQ-4bit` (4 GB, **e2b**, nhỏ hơn e4b). Muốn đo e4b cục bộ phải tải `mlx-community/gemma-4-e4b-it-4bit` (5,18 GB, `model.safetensors` 5,15 GB, từ huggingface.co) trong khi đĩa chỉ còn ~8 GB. Mọi số đo trên model thay thế phải ghi đúng tên model/runtime.
2. Chốt ngưỡng ở mục 3, và nếu phải phát hành trước khi đo: #3 giữ BẬT hay chuyển TẮT?
3. Gộp `fix/kanban-llm-loopback` (`b568796`) và sửa lỗi `finalize` đẩy card bị block sang Done (một dòng + test) — hai lỗi có sẵn, độc lập với project, đang được ghi trong `kanban-guide.md`?
4. Commit: tài liệu + công cụ đo này vào `feat/project`, cùng lúc với plan/báo cáo (để link không chết)?

---

# Bổ sung: đo trên model thay thế cục bộ (gemma-4-e2b-it-OptiQ-4bit qua sen-mlx)

## 7. Ngưỡng chốt TRƯỚC khi nhìn số (ghi lúc 2026-10-19 01:40, trước lần chạy đầu)

Người dùng (qua session cha) cho đo trên `mlx-community/gemma-4-e2b-it-OptiQ-4bit` do `sen-mlx` phục vụ, vì LM Studio `.93` tắt và máy không có weights e4b (không tải gì: đĩa còn ~8 GB). **Mọi số ở phần bổ sung này là của e2b/sen-mlx, KHÔNG phải gemma-4-e4b của người dùng.** e2b nhỏ hơn e4b nên rác (junk) nghiêng bi quan. Ngưỡng của mục 3, áp máy móc:

**Quyết định #3 (card xong tự lưu fact) đổi mặc định sang TẮT nếu bất kỳ điều nào sau đây đúng trên `real_extractor_card_summaries` (3 lần chạy):**
- (a) ≥ 2/3 lần chạy, một hoặc nhiều trong **8 summary "không có gì đáng lưu"** (`expect: none`: c07–c11 "Done."-kiểu, c14 sửa lỗi chính tả, c19 "Xong rồi.", c20 câu hỏi của worker) vẫn sinh ra fact (dòng `N/8 empty summaries left no fact` < 8/8);
- (b) tỉ lệ `junk candidates` / fact đã lưu, **sau khi tôi đọc tay và sửa các nhãn sai**, trung bình ≥ 30%;
- (c) bất kỳ fact nào, ở bất kỳ lần chạy nào, chứa chuỗi secret `sk_live`.
**Vùng lưng chừng** (junk trung bình 20–< 30%, hoặc (a) đúng đúng 1/3 lần): "chưa đủ căn cứ" ⇒ khuyến nghị TẮT. **Giữ BẬT** chỉ khi: (a) sai, junk < 20%, (c) sai. Quyết định theo luật của người dùng: trượt trên e2b ⇒ đổi sang TẮT (vì chưa có đo trên e4b và bật lại sau khi đo được là việc rẻ), ghi rõ trượt là trên e2b; vượt ⇒ giữ BẬT (bằng chứng mạnh vì e2b yếu hơn e4b).
Worker thật (8 card): chỉ **thông tin**, không tính vào ngưỡng: nếu phần lớn card không bao giờ gọi `kanban_complete` kèm summary thì retain gần như không chạy (vô hại, không rác).

**Quyết định #2 (fact cá nhân ở lại bank project)** không có công tắc trong mã; chỉ khuyến nghị: xem lại nếu ≥ 25% fact trong bank project là `personal`/`both` **và** bảng "hỏi từ chat ngoài project" cho thấy mất câu trả lời ở **scope theo chat** (mặc định). Nếu chỉ mất ở scope `agent` thì giữ (ghi trong docs).

## 8. Số đo trên gemma-4-e2b (sen-mlx) — 2026-10-19 01:42–02:26

**Model/runtime: `mlx-community/gemma-4-e2b-it-OptiQ-4bit`, `sen-mlx 0.1.1`, cổng 54931, `HOME`/`SENCLAW_HOME`/cwd tạm; client = `OpenAiCompatLlm` production (temperature 0.1, timeout 60 s). Đây KHÔNG phải e4b của người dùng**: e2b nhỏ hơn nên nhiễu bi quan; không suy ra cho e4b. RAM trống 76% (> 12 GB) trước khi chạy; không tải gì. Binary test: cây `e8b5264` + thay đổi tài liệu/test đo (chưa có thay đổi mặc định), chạy từ bản sao ở scratch. Log đầy đủ: [project-p5-measure/evidence/e2b-sen-mlx/](project-p5-measure/evidence/e2b-sen-mlx/); cỡ mẫu và số lần chạy ghi ở từng bảng.

### 8.1 A(1) — fact cá nhân trong bank project (12 cửa sổ: 5 dự án, 4 cá nhân, 3 lẫn; 7 câu cá nhân, 5 câu dự án; 3 lần chạy)

| | lần 1 | lần 2 | lần 3 |
|---|---|---|---|
| fact sống trong bank project | 17 | 14 | 16 |
| nhãn theo marker: dự án / cá nhân / lẫn / khác | 10 / 6 / 1 / 0 | 8 / 5 / 1 / 0 | 8 / 4 / 4 / 0 |
| **cá nhân + lẫn / tổng** | **7/17 = 41%** | **6/14 = 43%** | **8/16 = 50%** (gộp 21/47 = **45%**) |
| fact kiểu "Assistant đã ghi lại…" | 2 | 0 | 0 |
| hỏi 7 câu cá nhân từ **chính chat** đã nói: trước (chưa có project) / sau | 7/7 / 7/7 | 6/7 / 6/7 | 7/7 / 7/7 |
| từ chat **ngoài** project, scope theo chat (mặc định): trước / sau | 0/7 / 0/7 | 0/7 / 0/7 | 0/7 / 0/7 |
| từ chat ngoài project, scope `agent`: trước / sau | **7/7 / 0/7** | **6/7 / 0/7** | **7/7 / 0/7** |
| từ chat **khác của project**: sau (trước: không có) | 7/7 | 6/7 | 7/7 |
| 5 câu dự án từ chat khác của project / dự án lọt sang chat ngoài | 5/5 / 0 | 5/5 / 0 | 5/5 / 0 |
| đổi ý "cổng 8420→8431": fact cũ bị thay? | **không** (`superseded 0`) | không | không |
| số lần gọi model / trung vị | 12 / 5,1 s | 12 / 4,0 s | 12 / 12,3 s |

Đọc: (i) gần một nửa fact trong bank project là của cá nhân hoặc lẫn — extractor không tách (đúng điều R4 lo); (ii) với scope mặc định, mất mát ở chat ngoài project **bằng 0** (trước đó chat khác cũng không thấy fact của chat này); chỉ `SENCLAW_MEMORY_FACTS_SCOPE=agent` mất (7/7→0/7, 6/7→0/7); chat khác của project **được thêm** 6–7/7; (iii) không fact dự án nào lọt ra ngoài; (iv) e2b không bao giờ nêu `replaces`/slot cho ca đổi cổng: cả "chạy ở cổng 8420" lẫn "đã đổi từ 8420 sang 8431" cùng được recall (câu `port-now` bị chấm STALE 3/3 — dương tính thật, dù fact mới cũng chứa chữ 8420). Khớp phép đo cũ (0/2 lượt đổi ý trên chat thật).

### 8.2 A(2) — 28 summary card qua `retain` (3 lần chạy; mỗi lần ~6,5 phút, 28 lần gọi model)

| | lần 1 | lần 2 | lần 3 |
|---|---|---|---|
| fact lưu mới (`stored`) / `reinforced` (model chép lại fact đã biết) | 39 / 34 | 36 / 3 | 48 / 28 |
| **trong 8 summary "không có gì đáng lưu" (Done./Finished./câu hỏi/…), số summary vẫn sinh fact** | **8/8** | **8/8** | **7/8** |
| fact rỗng ruột kiểu "Kanban task X đã hoàn thành." (đếm tự động) | 11 | 7 | 18 |
| rác sau khi đọc tay (rỗng ruột + trùng + câu hỏi/việc vặt "expect none") | **15 = 38,5%** | **11 = 30,6%** | **21 = 43,8%** (gộp 47/123 = **38,2%**; trung bình ba lần 37,6%) |
| fact hữu ích | 24 | 25 | 27 |
| chunk hỏng | 1 (c23) | 0 | 0 |
| secret (`sk_live…`) lọt vào một fact | 0 | 0 | 0 (quét: 1 lần mỗi lần chạy) |
| 7 câu "worker kế tiếp nghe gì": trúng / giá trị cũ quay lại | 5/7 / 0 | 6/7 / 0 | 5/7 / 0 |
| trung vị gọi model | 15,0 s | 12,9 s | 10,3 s |

Luật đọc tay (chốt trước khi đọc lần 2–3): rác = câu chỉ nói "task X xong" không thêm gì; bản trùng nội dung của một fact đang sống; câu về trạng thái của một câu hỏi ("người dùng chưa quyết định…"); fact của card vốn "không có gì đáng lưu" (sửa chính tả README). Hữu ích = có chi tiết bền (số, đường dẫn, quyết định, nguyên nhân, ràng buộc) kể cả khi không chứa marker — nên các câu "Bước tiếp theo… expose metric", "miễn phí 40 lần chạy/tháng" được tính hữu ích (nghiêng về giữ BẬT). Độ nhạy: nếu bỏ ba mục "trùng/việc vặt/câu hỏi" ở lần 2 thì lần đó còn 8/36 = 22%; nhưng điều kiện (a) một mình đã quyết định.

Cơ chế nhiễu quan sát được:
1. **Chính văn bản retain mang một câu khẳng định**: `Kanban task "X" completed: …`. Với "Done." model rút ra đúng câu đó ("Kanban task 'Update the changelog' đã hoàn thành") — 8/8, 8/8, 7/8.
2. **Model chép lại `KNOWN FACTS` thành fact mới** khi gặp summary khác: `restated` không bắt được vì đổi ngôn ngữ/câu chữ. Lần 1 card c25 (chứa secret) lưu lại 2 fact của card c22/c17; lần 3 card c24 lưu 4 câu "Người dùng đã hoàn thành nhiệm vụ Kanban 'X'" với X là **card khác** (gán nhầm card).
3. Dịch/chuyển ngôn ngữ ngẫu nhiên (Việt ↔ Anh), nên cùng một ý có hai bản.
4. Chi tiết quan trọng đôi khi mất: c01 lần 1 chỉ lưu "Kanban task … đã hoàn thành." (mất "5 requests/phút", `rate_limit.rs`).
5. Chạy lặp card "second pass" (c23) sinh bản trùng ở 2/3 lần (lần 1: chunk hỏng).

### 8.3 A(3) — worker thật, 8 card tuần tự trên board của project (1 lần chạy; daemon `e8b5264`, `SENCLAW_DISPATCH_TIMEOUT_SECS=900`, proxy ghi log)

| Kết cục | Số card |
|---|---|
| `kanban_complete` kèm summary có nghĩa | **0** |
| `kanban_complete` summary rỗng | 0 |
| `kanban_block` | 0 |
| **dispatcher tự đóng (worker không gọi gì thành công)** → P3b không retain | **7** (W1, W2, W4, W5, W6, W7, W8) |
| treo tới hết giờ (902 s) → Blocked `gave_up` | 1 (W3: gọi `Skill`, `sbx_run` ×2, `sbx_exec`, rồi `AskUser` — không ai trả lời) |
| làm **đúng việc** (kiểm file/nội dung) | 5/8 (W1, W2, W4, W5, W8) |
| fact vào bank / yêu cầu retain / card nhận khối `<memory_context>` | 0 / 0 / 0 |

Nguyên nhân gốc: tool kanban **không có trong roster của worker** (33 tool, `ToolSearch` có sẵn; phải `select:` nạp trước). e2b **không gọi `ToolSearch` lần nào** (0/8): W1 gọi thẳng `kanban_complete` hai lần, W6 gọi `kanban_show` một lần — cả ba đều nhận "No such tool available… use ToolSearch"; 6 card không đụng tool kanban. Chỉ dẫn của dispatcher bảo "First call `mcp__senclaw-kanban__kanban_show`… MUST finish with kanban_complete/kanban_block" mà không nhắc nạp tool. Với model này, retain từ card **không bao giờ chạy**, bật hay tắt — nên nhiễu 8.2 là cận trên khi model mạnh hơn gọi được `kanban_complete`. W7 (card phải bị block): worker không làm gì và không block (dispatcher đóng Done). Hai phát hiện này thuộc kanban, không thuộc Project (mục 11).

## 9. Áp ngưỡng mục 7 một cách máy móc

| Điều kiện | Kết quả | Đúng? |
|---|---|---|
| (a) ≥ 2/3 lần chạy có summary rỗng vẫn sinh fact | 3/3 lần (8/8, 8/8, 7/8) | **ĐÚNG** |
| (b) rác sau đọc tay, trung bình ≥ 30% | 37,6% (gộp 38,2%); từng lần 38,5 / 30,6 / 43,8 | **ĐÚNG** |
| (c) secret trong một fact | 0 lần | sai |
| vùng lưng chừng (20–<30%) | không rơi vào | — |

⇒ **Quyết định #3: TẮT mặc định.** Trượt ngưỡng **trên e2b**; chưa có đo trên e4b, và bật lại sau khi đo được là việc rẻ (một biến môi trường). Tôi nêu rõ trượt là trên e2b: nếu e4b sạch hơn thì đảo lại; ba lệnh ở mục 12 đo được.
⇒ **Quyết định #2: giữ** ("ở lại bank project"). Điều kiện xem lại gồm hai vế: ≥ 25% fact cá nhân/lẫn (45%: đúng) **và** mất câu trả lời ở scope theo chat mặc định (0/7 trước và sau: sai) — chỉ scope `agent` mất. Docs đã ghi điều này (`projects.md` mục 8.6, 9). Nếu sau này muốn fact cá nhân xuyên project, cần cách đưa fact giữa bank (đã loại khỏi v1), không phải đổi mặc định.

## 10. Thay đổi mã (theo luật của người dùng: chỉ khi ngưỡng bảo TẮT)

- `src/config.rs`: `DispatchConfig.card_facts` (`SENCLAW_DISPATCH_CARD_FACTS`, mặc định **false**).
- `src/projects/facts.rs`: `worker_fact_target(project, enabled)` trả `None` khi `!enabled` — vẫn là chỗ duy nhất quyết; `src/projects/directory.rs`: `retain_target` truyền `config.dispatch.card_facts`.
- Test: `projects::facts` (thêm ca `enabled=false`), `projects::directory` (fixture nhận `Config`; ca cũ chạy với công tắc bật; ca mới `a_card_keeps_nothing_unless_the_daemon_was_told_to_keep_what_workers_finish`); `real_project.rs` đo với `enabled=true`. Không đụng `kanban/dispatch.rs` (nó chỉ gọi `ProjectDirectory::retain`).
- Docs khớp: `projects.md` (mục 6, 8.13, 9), `kanban-guide.md`, `memory-engine.md`, `CLAUDE.md` (gạch đầu dòng worker + "Projects"), `plan.md` (quyết định #3, P5), `design.md` §11 #3; harness P3b: `env-up.sh` đặt `SENCLAW_DISPATCH_CARD_FACTS=true` để chạy lại như viết.
- Kiểm: `cargo check --all-targets` sạch (2 cảnh báo có sẵn); `cargo test --lib` **3256 passed, 0 failed, 13 ignored** (3255 + 1 test mới). Daemon đo mục 8.3 là binary `e8b5264` (card facts còn **bật**), nên "0 retain" là thật chứ không do công tắc.

## 11. Phát hiện thêm (chưa sửa; ngoài phạm vi Project)

1. **Worker của kanban không nạp được tool kanban nếu model không biết dùng `ToolSearch`** (mục 8.3): 0/8 card tự đóng được. Sửa theo hướng nào là quyết định của người dùng: đặt tool kanban vào roster của worker (`skip_mcp_init` + danh sách cố định) hoặc thêm "load them with ToolSearch select:…" vào `KanbanDispatchProvider::guidance`. Ảnh hưởng mọi board, không riêng project; trên e2b làm cho dispatcher tự đóng *mọi* card.
2. **Worker có thể gọi `AskUser` và treo tới hết giờ** (W3, 902 s): worker chạy không có người trả lời; `SENCLAW_DISPATCH_TIMEOUT_SECS` là lưới an toàn duy nhất.
3. Extractor e2b không đổi ý (mục 8.1) và chép lại fact thành bản dịch (mục 8.2.2) — hai điểm yếu của model nhỏ đã biết trước, nay có số trên project.
4. Hai ghi chú về công cụ đo: câu `memory` (hỏi nguyên nhân tăng bộ nhớ, `must_include: LRU`) hỏi nhầm — "LRU" là cách sửa, không phải nguyên nhân, nên "missed" ở 3/3 lần không đáng tin; câu `port-now` chấm STALE vì fact mới cũng chứa "8420" (nhưng trường hợp thật vẫn đúng: fact cũ còn sống).

## 12. Chạy lại cho LM Studio / e4b (ba lệnh của mục 3 giữ nguyên)

Thêm hai chú ý: (1) daemon build từ cây hiện tại có card facts **tắt**; để đo retain từ card, truyền `SENCLAW_DISPATCH_CARD_FACTS=true` làm đối số thừa của `env-up.sh` (binary `e8b5264` ở scratchpad của P4 vẫn bật sẵn); (2) `exp-p5-worker.mjs` mặc định `P5_CARD_TIMEOUT=600`, đặt `SENCLAW_DISPATCH_TIMEOUT_SECS` tương ứng để card treo (AskUser) kết thúc. Script đọc log: `project-p5-measure/analyze-card-run.py <log summaries>` (đếm tự động; vẫn phải đọc tay các dòng REVIEW).

## 13. Môi trường và dọn dẹp

`sen-mlx` (pid riêng, cổng 54931), proxy 53931, daemon 53788/53789, mock 59931: **đã dừng**, cổng trống; chỉ đụng tiến trình do session này khởi động (các tiến trình `run-p4web/…` của session khác không đụng). Daemon thật 18788/18789 và `~/.senclaw` không bị chạm (chỉ đọc `config.json`/thư mục models; không in khoá). Không có build chạy chồng (`ps` kiểm trước mỗi `cargo`); khi đo không build để máy yên. Đĩa còn ~10 GB; không tải weights. Evidence 140 KB trong repo harness (log test, `result-p5-worker-e2b.json`, `proxy-requests-e2b.jsonl`; không có thân yêu cầu `bodies/`). Không commit.

## Chưa kiểm chứng (cập nhật)

- **Mọi số ở mục 8 là của gemma-4-e2b/sen-mlx, không phải `google/gemma-4-e4b` (LM Studio) của người dùng.** Chưa đo e4b; kết luận #3 là "TẮT cho tới khi đo e4b".
- 3 lần chạy mỗi phép đo, 12 cửa sổ / 7+5 câu / 28 summary / 8 card — mẫu nhỏ; phép đo worker chỉ 1 lần. Nhãn rác/hữu ích tự tôi đọc tay (luật ở 8.2, log để người khác kiểm lại).
- Worker với model **biết** nạp tool bằng `ToolSearch` chưa đo (nên chưa biết tỉ lệ `kanban_complete` kèm summary có nghĩa), embedding model thật chưa dùng (recall = từ khoá/thực thể/ngày), chỉ macOS.
- Docs mô tả REST/WS; giao diện web/desktop (P4) không nằm trong đó.

## Câu hỏi chưa giải quyết (cập nhật)

1. Bật máy `192.168.0.93` để chạy lại ba lệnh trên e4b thật — nếu e4b sạch hơn rõ rệt thì bật lại card facts (đổi mặc định hoặc cho từng project).
2. Sửa vấn đề worker không nạp được tool kanban (mục 11.1) và/hoặc `AskUser` treo (11.2)? Hai việc này quyết định liệu bất kỳ card nào tự lưu được fact, bất kể công tắc.
3. Gộp `fix/kanban-llm-loopback` (`b568796`) và sửa `finalize` đẩy card bị block sang Done — đã được yêu cầu **không** làm ở đây; vẫn ghi trong `kanban-guide.md`.
4. Công tắc card facts đang là **toàn daemon**. Cần theo từng project (cột mới `projects`)? Chỉ nên làm nếu e4b cho thấy bật được.
5. Commit tài liệu + công cụ đo + thay đổi mã này cùng plan/báo cáo (để link không chết) — session cha xử lý.
