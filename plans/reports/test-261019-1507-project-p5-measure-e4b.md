# Đo lại P5 trên google/gemma-4-e4b (LM Studio): card facts có bật lại được không?

Ngày 2026-10-19 14:08–15:10 (UTC+7) · Nền `aaff2bd` (`feat/project`, worktree `.worktrees/senclaw-project`, cây sạch) · **Không commit, không đổi mã, không đổi harness**
Tiếp nối: [test-261019-0112-project-p5-measure.md](test-261019-0112-project-p5-measure.md) (ngưỡng §7, luật đọc tay §8.2, số e2b §8) · [plan.md](261018-1618-project-feature/plan.md) P5, quyết định #2 và #3
Bằng chứng: [project-p5-measure/evidence/e4b-lm-studio/](project-p5-measure/evidence/e4b-lm-studio/) (6 log test, 2 lần worker, timeline)

## 0. KẾT QUẢ CHÍNH

**Model đo: `google/gemma-4-e4b`, LM Studio `http://192.168.0.93:1234/v1`, key rỗng (không in, không dùng khoá đám mây, không thay model).** `GET /v1/models` trả lời 200 ngay (5 model có sẵn; chỉ dùng e4b). Client = `OpenAiCompatLlm` production (temperature 0.1, timeout 60 s tính vào số đo), 0 lần gọi lỗi trong 120 lần gọi của các test.

**Quyết định #3: giữ TẮT. Không khuyến nghị bật lại.** Điều kiện bật lại (ngưỡng đã chốt trước ở §7 báo cáo cũ + tiêu chí của phiên này), áp máy móc:

| Điều kiện | e4b (3 lần chạy, 28 summary mỗi lần) | Đạt? |
|---|---|---|
| (a) 8 summary "không có gì đáng lưu" đều không sinh fact ở ≥ 2/3 lần | **8/8, 8/8, 8/8** summary vẫn sinh fact (0 summary rỗng sạch) | **KHÔNG** |
| (b) rác sau đọc tay < 20% (tiêu chí phiên này); ≥ 30% = trượt cứng, 20–<30% = "chưa đủ căn cứ ⇒ TẮT" (§7) | 25,0% / 33,3% / 29,4% — trung bình 29,2%, gộp 29/100 = **29,0%** | **KHÔNG** (vùng lưng chừng của §7, và ≥ 20%) |
| (c) không secret lọt vào fact | 0/3 lần (scrubber chặn 1 key mỗi lần) | đạt |

Cái e4b khác e2b (đều tốt hơn): không chép lại fact đã biết (`reinforced` 0/0/0, e2b 34/3/28), có dùng `replaces` khi đổi giá trị (3/3, e2b 0/3), câu "worker kế tiếp nghe gì" 7/7 ×3 (e2b 5–6/7), không có fact kiểu "Assistant đã…". **Nhưng điều chặn (a) không phải chất lượng model**: văn bản retain là `Kanban task "X" completed: <summary>` — chính nó đã là một câu khẳng định, nên với summary "Done." model nào cũng lưu lại đúng câu đó (e2b 8/8, 8/8, 7/8; e4b 8/8 ×3). Đổi sang model mạnh hơn không làm (a) qua được; phải đổi thứ đưa cho extractor (mục 5).

**Hai phát hiện ngoài phạm vi đo, ảnh hưởng thẳng đến #3:**
1. **Extractor bỏ qua cấu hình LLM có key rỗng** (`create_cognitive_llm`: "empty key disqualifies a remote config"). LM Studio của người dùng có key rỗng ⇒ trên cài đặt thật, **retain từ card không bao giờ chạy**, bật công tắc cũng vô tác dụng (log chỉ ở mức debug: "no extraction model configured; window not retained"). Tôi dính đúng chỗ này ở lần chạy worker thứ nhất: card W1 đóng bằng `kanban_complete` + summary mà không có yêu cầu retain nào tới model. Sửa đã có ở `.worktrees/senclaw-extractor-key` (chưa cài, theo ghi chú memory). Hai test `real_project` không dính vì dựng client trực tiếp.
2. **Worker e4b gần như không tự đóng card**: 2/16 card gọi `kanban_complete` (12,5%; e2b 0/8); 12/16 do dispatcher tự đóng; 2/16 treo ở `AskUserQuestion` tới hết giờ; 0/16 `kanban_block`. Một trong hai lần tự đóng là **dùng sai** (đóng Done với summary "Blocked waiting for Project Version…") và chính nó thành fact rác duy nhất lưu được từ worker thật, rồi bị đưa làm bộ nhớ duy nhất của card kế tiếp.

**Quyết định #2: giữ** (như e2b): 20/43 = 46,5% fact trong bank project là cá nhân/lẫn (e2b 45%); ở scope mặc định (theo chat) mất câu trả lời ở chat ngoài project = 0/21 trước và sau; chỉ `SENCLAW_MEMORY_FACTS_SCOPE=agent` mất (20/21 → 0/21).

## 1. Cách đo (đúng như §3/§12 báo cáo cũ)

- **Test lib**: `cargo test --lib --no-run` (`CARGO_TARGET_DIR=…/senclaw/target`, `CARGO_INCREMENTAL=0`) tại `aaff2bd`, chỉ chạy khi không có rustc nào của tôi; một session khác (`.worktrees/senclaw-release`) build cùng hash `senclaw-4dcb4595fe0d1c80` đè lên nhau nên tôi sao binary ra scratch ngay (sha256 `339907f0…`) và kiểm `--list --ignored` thấy đúng hai test `real_project`. Chạy từ bản sao, `HOME`/`SENCLAW_HOME`/cwd tạm riêng từng lần, `SENCLAW_EVAL_LLM_BASE_URL=http://192.168.0.93:1234/v1`, `SENCLAW_EVAL_LLM_MODEL=google/gemma-4-e4b`. Chạy tuần tự (không song song, để độ trễ không bị méo): mỗi test 3 lần, xen kẽ.
- **Worker thật**: `env-up.sh` + `exp-p5-worker.mjs` (8 card/lần, 2 lần = **16 card**), daemon cô lập cổng 53788/53789 + proxy ghi log 53931 (không 18788/18789/18790, không `~/.senclaw`; Seatbelt chặn kết nối tới ba cổng thật). Binary `p4-daemon/senclaw` nêu trong đề **không còn** (scratchpad đã dọn); thay bằng bản **sao** của `target/debug/senclaw` (02:57, sha256 `69d172e5…`; chỉ đọc, không ghi đè cái người dùng có thể chạy lại): `strings` thấy `SENCLAW_DISPATCH_CARD_FACTS` và `/api/projects`, và `git diff --stat e8b5264 aaff2bd -- src` chỉ có công tắc này + test đo. Chạy với `SENCLAW_DISPATCH_CARD_FACTS=true`, `SENCLAW_DISPATCH_TIMEOUT_SECS=480`, `P5_CARD_TIMEOUT=600`. **Không xác minh được cây nguồn của binary 02:57 đúng bằng `aaff2bd`** (chỉ suy từ nội dung + diff).
- **Lần worker 1 (`nokey`)**: cấu hình y hệt cài thật (key rỗng) ⇒ extractor bị bỏ qua (mục 4). **Lần 2 (`key`)**: cùng daemon, chỉ khác `apiKey` = một chuỗi giữ chỗ trong `config.json` tạm (LM Studio bỏ qua; proxy trên loopback; không phải khoá thật; chuỗi không nằm trong evidence — đã `grep`), để retain chạy được. Cả hai lần đều báo trong bảng.
- Luật đọc tay: **y như §8.2** (rác = fact chỉ nói "task X xong"; trùng nội dung một fact đang sống; câu về trạng thái một câu hỏi; fact của card vốn "không có gì đáng lưu"; hữu ích = chi tiết bền kể cả khi không chứa marker). Áp bằng script (`label-cards.py` ở scratch: 8 card `expect none` ⇒ mọi fact rác; fact ≤ 2 từ nội dung ngoài tiêu đề ⇒ rác; bản chép lại của c22 khi fact c22 còn sống ⇒ rác) rồi tôi **đọc cả 3 log** để kiểm: script đổi nhãn của công cụ ở c15/c26/c03 (công cụ gọi nhầm là `JUNK?` vì không chứa marker nhưng có chi tiết bền) và c23 lần 3 (trùng). Khác với e2b: ở e4b không có fact chép lại `KNOWN FACTS` nên không có loại "trùng" nào khác phải đếm tay. "Lan vắng thứ Sáu" (c26) tính hữu ích (cụ thể, card `expect maybe`; nghiêng về giữ BẬT như §8.2) — tính là rác thì rác thành 27,8 / 36,7 / 32,4% (gộp 32%), kết luận không đổi.

## 2. A(1) — fact cá nhân trong bank project (12 cửa sổ: 5 dự án, 4 cá nhân, 3 lẫn; 7 câu cá nhân + 5 câu dự án; 3 lần chạy)

| | lần 1 | lần 2 | lần 3 | gộp / ghi chú |
|---|---|---|---|---|
| fact sống trong bank project | 14 | 14 | 15 | 43 |
| nhãn marker: cá nhân / dự án / lẫn / khác | 6 / 7 / 1 / 0 | 5 / 7 / 1 / 1 | 6 / 7 / 1 / 1 | "khác" = hạn bàn giao viết `2026-11-15` / `15 tháng 11`, marker `15/11` không khớp: **là fact dự án** (đọc tay) |
| **cá nhân + lẫn / tổng** | **7/14 = 50%** | **6/14 = 43%** | **7/15 = 47%** | **20/43 = 46,5%** (e2b 21/47 = 45%) |
| fact kiểu "Assistant đã ghi lại…" | 0 | 0 | 0 | e2b 2 / 0 / 0 |
| 7 câu cá nhân hỏi từ chính chat đã nói: trước (chưa có project) / sau | 7/7 / 7/7 | 6/7 / 6/7 | 7/7 / 7/7 | lần 2 mất "vợ tên Lan" (extractor bỏ phần cá nhân của cửa sổ lẫn m2) — mất ở cả hai cột |
| từ chat **ngoài** project, scope theo chat (mặc định): trước / sau | 0/7 / 0/7 | 0/7 / 0/7 | 0/7 / 0/7 | **không mất gì** (fact theo chat vốn không sang chat khác) |
| từ chat ngoài project, scope `agent`: trước / sau | 7/7 / **0/7** | 6/7 / **0/7** | 7/7 / **0/7** | mất ở scope `agent` (giống e2b) |
| từ chat **khác của project**: sau (trước: không có) | 7/7 | 6/7 | 7/7 | được thêm |
| 5 câu dự án từ chat khác của project: công cụ báo / sau đọc tay | 5/5 / 5/5 | 4/5 / 5/5 | 4/5 / 5/5 | "deadline missed" ở lần 2–3 là marker `15/11` không khớp định dạng ngày e4b viết; fact có đủ. Dự án lọt sang chat ngoài: 0 ×3 |
| **đổi ý cổng 8420→8431: fact cũ bị thay?** | **có** (`superseded 1`) | **có** (viết lại thành "cổng 8431") | **có** (`superseded 1`) | **3/3** (e2b 0/3). Cờ `port-now STALE` ở lần 1 và 3 là của công cụ: fact mới "đã đổi từ 8420 sang 8431" tự chứa "8420"; bank cuối **không còn** fact "chạy ở cổng 8420" |
| gọi model / trung vị | 12 / 8,3 s | 12 / 8,0 s | 12 / 9,2 s | 0 lỗi; mỗi lần 99–113 s tổng (e2b trung vị 4–12 s) |

Đọc: (i) e4b cũng không tách được cá nhân khỏi dự án (≈ nửa bank là cá nhân/lẫn) — R4 vẫn đúng; (ii) với scope mặc định mất mát ở chat ngoài project bằng 0; (iii) khác e2b: e4b **thay đúng giá trị đã đổi** 3/3, nên "stale value" ở project không còn là rủi ro đo được trên e4b. ⇒ **#2 giữ** (điều kiện xem lại gồm hai vế §9: ≥ 25% cá nhân — đúng; **và** mất ở scope mặc định — sai).

## 3. A(2) — 28 summary card qua `retain` (3 lần chạy; 4:31–5:09 mỗi lần, 28 lần gọi model, 0 lỗi)

| | lần 1 | lần 2 | lần 3 | gộp |
|---|---|---|---|---|
| fact lưu mới / `reinforced` | 36 / 0 | 30 / 0 | 34 / 0 | 100 / 0 (e2b 123 / 65) |
| fact sống (sau `superseded`) | 35 | 29 | 34 | |
| **trong 8 summary "không có gì đáng lưu", số summary vẫn sinh fact** | **8/8** | **8/8** | **8/8** | (e2b 8/8, 8/8, 7/8) |
| rác sau đọc tay | **9 = 25,0%** | **10 = 33,3%** | **10 = 29,4%** | **29/100 = 29,0%** (TB 29,2%; e2b 38,2%) |
| fact hữu ích | 27 | 20 | 24 | 71 |
| rác chia theo nguồn | 8 từ 8 card "none" + 1 "Summarise the standup notes completed" | 9 từ card "none" (c20 sinh 2) + 1 như trên | 8 từ card "none" + 1 như trên + c23 trùng c22 | 25 từ card "none", 3 "task X xong", 1 trùng |
| chunk hỏng / secret lọt vào fact | 0 / 0 | 0 / 0 | 0 / 0 | scrubbed `stripe_key` 1/lần |
| 7 câu "worker kế tiếp nghe gì": trúng / giá trị cũ quay lại | 7/7 / 0 | 7/7 / 0 | 7/7 / 1 | (e2b 5–6/7 / 0). Lần 3: cả "page size 50" (c22 + bản trùng c23, không bị thay) và "25" (c24) còn sống ⇒ 1 câu bị chấm có giá trị cũ |
| `superseded` | 1 (c23 thay c22) | 1 (c24 thay c22) | 0 | e2b 0 |
| trung vị gọi model | 9,9 s | 10,1 s | 10,3 s | (e2b 10–15 s) |

Cơ chế (khớp §8.2.1): cả 8 card "Done." / "Finished." / "Task completed successfully." / "Xong rồi." / câu hỏi của worker đều thành đúng một câu `The Kanban task "X" was completed.` (c20 lần 2 thành hai: "người dùng chưa đổi gì" + "cần quyết định cursor hay offset"). Phần còn lại của rác là nhỏ: 1 tautology ở c26, 1 bản trùng c23 (lần 3).
**Nếu 8 summary rỗng không bao giờ tới extractor** (tính từ chính các log này, **không phải phép đo của một bộ lọc đã xây**): rác còn 1/28, 1/21, 2/26 = 3,6 / 4,8 / 7,7% (gộp 4/75 = 5,3%) và (a) qua theo cấu tạo. Điều chưa biết là một luật rẻ có tách được 8 summary đó khỏi các summary ngắn mà hữu ích ("Port is 9464.", "Bumped axios to 1.7.2.", "removed 31 unused imports") hay không — c14 "Fixed a typo in README.md." có đường dẫn.

## 4. A(3) — worker thật: 16 card (2 lần × 8 trên board của project, tuần tự)

| Kết cục | lần 1 `nokey` | lần 2 `key` | tổng /16 | e2b /8 |
|---|---|---|---|---|
| `kanban_complete` + summary | 1 (W1) | 1 (W6) | **2** | 0 |
| `kanban_complete` summary rỗng / `kanban_block` | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| **dispatcher tự đóng (worker không gọi tool kanban nào thành công)** | 6 (W2 W3 W4 W5 W7 W8) | 6 (W1–W5, W8) | **12** | 7 |
| treo ở `AskUserQuestion` tới hết giờ (480 s) → Blocked bởi dispatcher | 1 (W6) | 1 (W7) | **2** | 1 (902 s) |
| card gọi `ToolSearch` | 1 (W1, tìm ra) | 2 (W1: 4 lần không ra; W6: ra) | **3** | 0 (0/8) |
| việc làm đúng (đọc tay câu trả lời cuối / file) | 6 (W1–W5, W8) | 5 (W1–W5) | **11** | 5 |
| công cụ chấm đúng (tự động) | 5 | 5 | 10 | |
| fact vào bank / yêu cầu retain tới model | 0 / 0 (extractor bỏ qua, key rỗng) | 1 / 1 | 1 / 1 | 0 / 0 |
| card nhận khối `<memory_context>` | 0 | 1 (W8) | 1 | 0 |

- **Công cụ chấm sai ở W3 cả hai lần** (đáp án cuối đúng: 12 đơn, tổng 1644): driver chỉ kiểm `summary` của dispatcher, vốn là **dòng đầu** của câu trả lời cuối ("Based on the analysis…"). Đã sửa tay trong cột "việc làm đúng". Không đổi harness (lưu ý cho lần sau).
- **W7 (card phải bị block): 0/2 đúng quy trình.** Lần 1 worker từ chối bằng chữ, không gọi gì, dispatcher đóng **Done**; lần 2 gọi `AskUserQuestion` và treo 483 s (Blocked chỉ vì hết giờ).
- **W6 lần 2 (đóng card bằng `kanban_complete` khi thật ra bị kẹt)**: worker tìm VERSION sai chỗ, gọi `mcp__kanban__list/link/list…`, rồi `complete(summary="Blocked waiting for Project Version. Please provide…")` ⇒ card thành **Done**, và retain lưu fact `Người dùng cần số phiên bản hiện tại của Sao Kim để hoàn thành nhiệm vụ Kanban…` (trạng thái của câu hỏi — rác theo luật). Fact đó là **bộ nhớ duy nhất** W8 nhận ("Which port does the backend use"), W8 không tìm ra cổng (Glob/Grep sai pattern) ⇒ trên mẫu này fact từ card thật không giúp card sau mà làm nhiễu. Một mẫu — không suy rộng.
- **Vì sao ít tự đóng**: tool kanban không nằm sẵn trong roster (phải nạp bằng `ToolSearch`), và hướng dẫn của dispatcher ghi tên `mcp__senclaw-kanban__kanban_show/complete` — tên đó **không** khớp bằng truy vấn từ khoá: ở W1 lần 2 model yêu cầu 4 truy vấn `ToolSearch` (`kanban***`, `mcp__senclaw-kanban__kanban_show` ×2, `mcp__senclaw-kanban__kanban_complete`); daemon thực thi 3, không lần nào "discover" tool nào. Các truy vấn tự nhiên (W1 lần 1 "kanban completion"; W6 lần 2 "kanban…") tìm ra `mcp__kanban__complete`. W1 (cả hai lần) còn gọi nhầm `Task{card_id,summary}` trước khi `ToolSearch`, và gọi `Write` lặp 2–4 lần (bị chặn "Duplicate call"). Thuộc kanban, không thuộc Project (§11.1 báo cáo cũ — nay có số trên e4b).
- Độ trễ: 65 yêu cầu worker (29 + 36), trung bình ≈ 10 s mỗi yêu cầu (294,8 s / 29; 338,8 s / 36), 0 lỗi HTTP.

## 5. Khuyến nghị

1. **Giữ #3 TẮT.** Không đổi mã, không đổi tài liệu về mặc định (theo yêu cầu: người dùng quyết). `plan.md` đã ghi kết quả e4b.
2. **Đổi model không đủ**; muốn #3 có lợi cần ba việc, chưa việc nào được xây/đo: (i) sửa extractor bỏ qua key rỗng (`.worktrees/senclaw-extractor-key`) — không thì bật công tắc trên cài đặt thật cũng không ghi gì; (ii) thay đổi cái đưa cho extractor: bộ lọc trước `retain_completion` bỏ summary chung chung/ngắn, hoặc bỏ câu dẫn "Kanban task X completed:" và bảo extractor trả `[]` khi không có gì bền — rồi chạy lại `real_project` (≈ 5 phút/lần, 3 lần) với cùng 28 summary; ước tính từ log cũ cho thấy rác có thể còn ~5%; (iii) để worker đóng được card (tool kanban vào roster của worker, hoặc hướng dẫn ghi `ToolSearch select:…`), nếu không thì 75% card không bao giờ retain.
3. Cũng nên cân nhắc chặn worker dùng `AskUserQuestion` (2/16 card treo 8 phút) và chặn "đóng Done bằng summary kiểu bị kẹt" — hai lỗi kanban độc lập với Project.
4. **#2 giữ** (mục 2).

## Chưa kiểm chứng

- Mẫu nhỏ: 3 lần × (12 cửa sổ / 28 summary), 16 card (2 lần). Nhãn rác/hữu ích do tôi đọc tay (luật §8.2, log để người khác kiểm lại; script gán nhãn ở scratch, không đưa vào repo). Chỉ **một** fact từ worker thật được lưu (rác) — chất lượng retain từ **summary tốt của worker thật** chưa quan sát (W1 lần 1 tự đóng với summary tốt nhưng extractor bị bỏ qua vì key rỗng).
- Phép đo worker lần 2 dùng key giữ chỗ để mở khoá extractor — **không phải** hành vi cài đặt thật (key rỗng ⇒ không retain).
- Embedding model thật chưa dùng (recall = từ khoá/thực thể/ngày), không `thinkingEnabled` khác cài thật; chỉ macOS. Binary daemon 02:57 chưa xác minh bằng cây nguồn (mục 1). LM Studio có thể đang phục vụ model khác cùng lúc (không biết); độ trễ chỉ để tham khảo.
- Docs mô tả mặc định TẮT và "đo trên e2b" — **chưa sửa** (theo yêu cầu); `projects.md`/`kanban-guide.md`/`memory-engine.md`/`CLAUDE.md` còn câu "chưa đo e4b", cần đổi khi người dùng chốt.

## Dọn dẹp

Hai daemon cô lập (53788/53789), proxy (53931) và các tiến trình test đều đã dừng; cổng 53788/53789/53931 trống (kiểm `lsof` + `ps`). Daemon thật 18788/18789 (pid của người dùng) không bị lệnh nào của tôi nhắm tới; daemon cô lập chạy dưới Seatbelt chặn kết nối tới ba cổng thật. Không `cargo fmt`, không commit; chỉ ghi vào `senclaw/plans/` (báo cáo này, `plan.md`, `evidence/e4b-lm-studio/`) và thư mục scratch của session; `target/debug/senclaw` chỉ được **đọc** (sao ra scratch). Sau khi build xong và không còn rustc nào, tôi xoá các `senclaw-4dcb4595fe0d1c80.*.rcgu.o` còn lại (đĩa trống 3,4 → 4,2 GB); file binary `senclaw-4dcb4595fe0d1c80` trong `target/debug/deps` lúc cuối **không còn** (không do lệnh xoá của tôi — tôi chỉ xoá `*.rcgu.o`; có thể một session khác link lại), bản sao của tôi vẫn ở scratch.

## Câu hỏi chưa giải quyết

1. Làm tiếp (iii) mục 5.2? — thử bộ lọc summary + sửa extractor key rồi đo lại 28 summary (một session riêng, ~1 giờ), hay giữ #3 TẮT và thôi?
2. Gộp sửa extractor-key (`.worktrees/senclaw-extractor-key`) vào bản cài thật? Nó ảnh hưởng mọi retain (chat, compaction, card), không riêng #3.
3. Sửa kanban: tool vào roster của worker / tên trong hướng dẫn / chặn `AskUserQuestion` — theo hướng nào?
