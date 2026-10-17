# Eval: model trích xuất thật cho kho fact

Ngày 2026-10-17 · nhánh `feat/memory-engine` · `src/memory/engine/eval.rs` (`real_extractor_keyword_only`, `real_extractor_with_vectors`, `#[ignore]`) · cách chạy: [`docs/memory-engine.md`](../../docs/memory-engine.md) "Chạy eval".

## Cách đo

- 7 cửa sổ `fact_windows` + 7 `fact_queries` của fixture, qua **retain thật** với model thật thay `ScriptedFacts`. Client là `OpenAiCompatLlm` của production (temperature 0.1, timeout 60 s, `json_object`).
- Model: `gemma-4-e2b-it-OptiQ-4bit` (4-bit) qua `sen-mlx 0.1.1` chạy tay trên `127.0.0.1:38931`, `HOME`/`SENCLAW_HOME`/cwd tạm. Không chạm daemon 18788/18789.
- **Không đo được `gemma-4-e4b`** (model người dùng cấu hình, LM Studio `192.168.0.93:1234`): máy đó không trả lời lúc đo. Số dưới đây là của 2B, không suy ra e4b.
- Kết quả chính: 8 lần chạy cuối (4 keyword-only, 4 embedder băm từ), 56 lần gọi model, chấm bằng harness cuối cùng. 10 lần chạy sớm hơn (chấm leak yếu hơn) chỉ dùng cho đếm `replaces` và JSON hỏng.
- Embedder băm từ chỉ đo cơ chế dedupe, không nói gì về embedding thật.

## Kết quả (8 lần chạy cuối)

| | keyword-only (4) | embedder băm từ (4) |
|---|---|---|
| câu đúng, chuỗi chính xác (trên 7) | 5, 5, 5, 6 | 5, 6, 6, 6 |
| câu đúng khi bỏ dấu tiếng Việt | 6, 6, 6, 7 | 6, 7, 7, 7 |
| câu đúng không có kho fact | 0 | 0 |
| `LEAKED` | 0 | 1 lần (ca đổi ý) |
| `replaces` nêu / chấp nhận / bỏ qua | 4 / 4 / 0 | 3 / 3 / 0 (1 lần model không nêu) |
| fact trích / lưu / tăng chứng (fixture kỳ vọng 8) | 10–16 / 10–13 / 0–4 | 12–22 / 10–13 / 2–9 |
| trả lời hỏng JSON | 1 / 28 | 0 / 28 |

Độ trễ mỗi lần gọi: median 11.5 s, 2.5–24.2 s; tăng theo số dòng `KNOWN FACTS`. Cao nhất 24.2 s ở 8 lần cuối, 22.4 s ở 10 lần trước: không lần nào chạm timeout 60 s, không lỗi HTTP. Secret `sk-proj-…` không lần nào tới prompt.

Câu trượt:

- `yesterday-vi` 8/8 lần theo chuỗi chính xác: model viết "xóa cookie cũ", fixture "xoá cookie cũ". Fact vẫn đứng hạng 1; đúng khi bỏ dấu. Lỗi chấm, không phải lỗi trích.
- `month-vi` 2/8 (keyword-only): "tháng 3 năm nay" thành `when 2024-03`, sai năm (TODAY là 2026).
- `secret-in-the-chat` 1/8: trả lời hỏng JSON, mất cả cửa sổ Orion.
- `changed-mind-react-vue` 1/8 trượt + `LEAKED`: xem mục 1.

## Đọc kết quả

1. **`replaces` đáng tin khi model nêu, nhưng không phải lần nào nó cũng nêu.** Gộp 18 lần chạy: nêu 15, chấp nhận 15, bỏ qua 0, không lần nào trỏ sai. 2 lần model không nêu gì; 1 lần trả lời cửa sổ Vue hỏng JSON. Lần tệ nhất (embedder, lần 1): model **đọc lại cả 7 dòng `KNOWN FACTS`** thay vì trích fact mới, `replaces` rỗng, fact "thích sử dụng React" còn sống và `LEAKED` cạnh Vue. Fact React có trong danh sách (là `[F1]`) nên lỗi là của model, không phải của truy vấn `KNOWN FACTS`.
2. **Chấm theo cụm của fixture bỏ sót đúng lỗi này.** 7 trên 18 lần model viết "thích **sử** dụng React", không khớp "thích dùng React"; lần 1 ở trên sẽ đọc là `leaked 0`. Harness giờ theo dõi fact cũ theo câu chữ của model (trích từ đúng cửa sổ cũ, cùng thực thể) và tính `LEAKED` nếu nó còn sống.
3. **~3% trả lời hỏng JSON** (4 trên 133 lần gọi): 2 thiếu dấu đóng ở cuối (`}` hoặc `]}`), 1 thừa `}`, 1 chưa xem kỹ (lần chạy đầu, bị cắt khi in). `parse_facts` cắt từ `{` đầu tới `}` cuối nên cả hai kiểu đều thành `failed_chunks` và mất nguyên cửa sổ, không cứu được fact nào.
4. **Model hay nhắc lại fact đã biết** dù prompt cấm: trích 10–22 fact cho 8 fact kỳ vọng. Có embedder thì phần lớn thành `reinforced` (2–9 mỗi lần). **Cài đặt mặc định (không embedding) chỉ dedupe khi trùng nguyên văn**, nên bản diễn đạt lại được lưu thành fact mới: kho keyword-only cuối run có 10–13 fact thay vì 8 (việc "sửa lỗi đăng nhập" nằm 1–3 lần).
5. **Ngày giờ**: sai năm 2/8; từ tương đối ("hôm qua", "năm nay") còn nằm trong chữ của fact 0–5 fact mỗi lần chạy, và sẽ sai sau vài tháng dù `when` đúng.
6. **Chi phí ghi**: ~10 s mỗi chunk trên máy này. Ngoài đường đọc nên không làm chậm lượt chat, nhưng một lần flush cửa sổ n chunk là n × ~10 s.

## Hạn chế

- 2B không phải e4b; 7 cửa sổ, 7 câu, 8 lần chạy: chỉ đủ để thấy kiểu lỗi, không đủ cho tỉ lệ chính xác. Câu hỏi được viết sau khi biết fact.
- Phát hiện fact cũ dựa vào thực thể chung với fact trong fixture; model đặt thực thể khác thì ca đó đọc là "không thử gì về `replaces`".
- Cột "bỏ dấu" chỉ nới việc so chuỗi, chuỗi tìm vẫn là của fixture.

## Việc tiếp theo (chưa làm, ngoài phạm vi phiên này)

- `facts_array` ([`extract.rs`](../../src/memory/engine/facts/extract.rs)): đọc giá trị JSON đầu tiên hoàn chỉnh, bù `]}` thiếu. Đo lại bằng `real_extractor_*`.
- Lọc fact trích ra mà gần như trùng một dòng `KNOWN FACTS` trước khi lưu (`retain.rs`), nhất là ở chế độ không embedding.
- Chặn từ tương đối trong chữ của fact / kiểm tra năm so với TODAY.
- Chạy lại trên `gemma-4-e4b` khi máy LM Studio lên.

## Câu hỏi chưa giải quyết

- Có chạy trên e4b thật (LM Studio `192.168.0.93:1234`) không, hay xem 2B là đại diện đủ cho "model nhỏ"?
- `parse_facts` có nên tự sửa dấu đóng JSON (đổi hành vi parser đã commit), hay để model-prompt xử lý?

## Sau khi sửa: `facts_array` + lọc fact nhắc lại

Cùng ngày, cùng máy, cùng model (`gemma-4-e2b` 4-bit, `sen-mlx 0.1.1` scratch trên `127.0.0.1:38931`, `HOME`/`SENCLAW_HOME`/cwd tạm). Đo lại bằng `real_extractor_*`: 5 lần keyword-only + 5 lần embedder băm từ, 70 lần gọi model. "Trước" là 8 lần chạy cuối ở trên (4 + 4, 56 lần gọi). Hai đợt không chạy cùng lúc và model đổi câu trả lời giữa các lần: số nhỏ, chỉ đủ thấy hướng.

Thay đổi:

- [`extract.rs`](../../src/memory/engine/facts/extract.rs) `facts_array`: đọc **giá trị JSON hoàn chỉnh đầu tiên** (thừa `}`/`]`/prose ở sau thì bỏ qua); không đọc được thì bù dấu đóng còn thiếu — ở cuối, hoặc ngay trước dấu đóng của vòng ngoài khi model làm rơi `]` của mảng (`…"entities":["user"]} }`). Không bù khi câu trả lời dừng giữa một chuỗi (sẽ lưu nửa câu); `{"triplets":[]}` vẫn là lỗi.
- [`restated.rs`](../../src/memory/engine/facts/restated.rs) (mới) + [`retain.rs`](../../src/memory/engine/facts/retain.rs) `store_one`: fact mới chỉ nói lại một dòng `KNOWN FACTS` của chunk nó thì `touch` dòng cũ, tính `reinforced`, không lưu. Chạy trước đường vector nên cả hai chế độ đều dùng. Fact cũ chưa có ngày mà bản nhắc lại có `when` thì nhận ngày (`FactStore::date_if_missing`).
- Test: 21 test mới, `cargo test --lib memory::engine` 144 pass. Ba kiểu JSON hỏng của báo cáo này nằm trong test nguyên văn từ log; thêm kiểu thứ tư (rơi `]`) thấy ở lần đo lại.

Luật "nhắc lại" (đủ cả năm): không có `replaces`; ≥ 3 từ và ≥ 80% từ của fact mới đã có trong fact cũ (`user`/`người dùng`/`assistant` không tính); số/ngày, từ phủ định và tên riêng viết hoa của fact mới đều có trong fact cũ và fact mới không bỏ mất "không" của fact cũ; entity model liệt kê còn quá nửa số từ ở phía kia; `when` trùng hoặc fact cũ chưa có ngày. Không dùng `overlap_coefficient` đơn thuần: nó chia cho vế ngắn nên fact dài chứa fact ngắn đã lưu được 1.0 dù thêm ý. Ngưỡng 0.8 chọn trên log cũ: trong 29 cặp diễn đạt lại thật, 28 đạt ≥ 0.8 (sau khi bỏ chủ ngữ chung) và 1 ở 0.75 (bản có thêm vế mới, nên giữ); ca đổi ý React → Vue là 0.25. Dữ liệu chỉ có hai cụm xa nhau, nên 0.8 là lề chứ không phải ranh giới được chứng minh. Các ca "đổi một thứ" (tên, số, ngày, phủ định) bị chặn bởi luật danh tính chứ không bởi ngưỡng: "thích dùng Vue" so với "thích dùng React" vẫn đạt 0.8.

| | keyword trước (4) | keyword sau (5) | vector trước (4) | vector sau (5) |
|---|---|---|---|---|
| trích / lưu / tăng chứng, trung bình mỗi lần chạy | 13.0 / 11.5 / 1.5 | 14.6 / 10.0 / 4.6 | 17.8 / 11.5 / 6.3 | 13.4 / 10.8 / 2.6 |
| `reinforced` từng lần | 0, 2, 0, 4 | 7, 4, 6, 6, 0 | 9, 5, 2, 9 | 0, 8, 0, 1, 4 |
| fact còn sống cuối lần chạy (fixture: 8) | 10.5 | 9.0 | 10.8 | 10.0 |
| cặp fact sống nói lại nhau (TB)¹ | 2.0 | 0.2 | 1.25 | 1.2 |
| "sửa lỗi đăng nhập" còn sống (TB) | 2.25 | 1.2 | 2.25 | 2.0 |
| câu đúng, chuỗi chính xác (TB trên 7) | 5.25 | 5.6 | 5.75 | 5.0 |
| câu đúng khi bỏ dấu (TB trên 7) | 6.25 | 6.6 | 6.75 | 6.0 |
| `LEAKED` | 0 / 4 | 0 / 5 | 1 / 4 | 1 / 5 |
| trả lời hỏng JSON | 1 / 28 | 0 / 35 | 0 / 28 | 1 / 35 |
| `replaces` nêu / chấp nhận | 4 / 4 | 5 / 5 | 3 / 3 | 4 / 4 |

¹ Đếm ở kho cuối bằng bản Python của luật trên (không có entity), cùng một hàm cho trước và sau.

Đọc kết quả:

1. **Keyword-only (cài đặt mặc định) là chỗ có tác dụng.** `reinforced` 1.5 → 4.6 mỗi lần chạy, cặp nói lại nhau 2.0 → 0.2, bản "sửa lỗi đăng nhập" 2.25 → 1.2. Kho vẫn 9.0 fact thay vì 8 vì chuyện khác: câu xã giao ("Người dùng xác nhận thông tin…") và hai fact cùng một câu trả lời (bản ngắn + bản dài "Hôm qua…"; luật này chỉ so với `KNOWN FACTS`, không so các fact trong cùng một câu trả lời).
2. **Embedder băm từ gần như không đổi** (cặp nói lại nhau 1.25 → 1.2): vector đã bắt phần lớn từ trước, phần còn lại là các cặp cùng-câu-trả-lời nói trên. `reinforced` giảm 6.3 → 2.6 vì trong đợt sau model ít nhắc lại hơn (trích 13.4 thay vì 17.8), không vì luật này bắt ít đi.
3. **Không ca đổi nào bị nuốt trong log.** Chạy bản Python của luật trên các cặp (fact trích, `KNOWN FACTS`) của 10 lần chạy sau: 14 cặp diễn đạt lại bị bỏ — "User" ↔ "Người dùng", thêm "đã"/"bắt đầu", "API key" ↔ "khóa API", câu tách đôi, "vào hôm qua" dời chỗ — soát tay cả 14, đều là nhắc lại; React → Vue vẫn được lưu và thay, `replaces` 9/9 lần model nêu. Hai vector-run trượt câu: một lần model không nêu `replaces` (`LEAKED`, như 1 trong 4 trước đó), một lần mất cả cửa sổ Orion vì JSON hỏng (`secret-in-the-chat`).
4. **JSON hỏng: kiểu thiếu/thừa dấu đóng hết, còn một kiểu khác.** Trước: 4/133 lần gọi (3.0%). Sau: 1/70 (vector-5), kiểu **khoá bị lặp** `{"text{"text":"…"` (xuất hiện hai lần trong cùng câu trả lời) — không phải dấu đóng, vẫn mất cả cửa sổ. Bản đầu chỉ bù ở cuối, đo 70 lần gọi còn 2 hỏng: một kiểu mới (`…"entities":["user"]} }`, thay `]}` bằng ` }`, nay đã bù) và một khoá lặp. Harness không in lần nào việc sửa chạy, nên đo sống chỉ cho thấy "không còn hỏng kiểu dấu đóng", không đếm được số lần cứu; bằng chứng cho việc sửa là test đơn vị trên câu trả lời thật.
5. **Hai cửa sổ trả lời rỗng** (`{"facts":[]}` ở cửa sổ "đổi cổng", keyword-2 và keyword-5, ~1.5 s, nên mất fact cổng 4802 và trượt `assistant-did-this-week`). Xảy ra trước retain, nên không do luật này; trong 203 lần gọi trước đó (kể cả đợt đo giữa chừng) không có lần nào. Có thể là nhiễu của model, 2 trên 70 thì chưa kết luận được.
6. Độ trễ model không đổi (median 8–13 s); ngày giờ (sai năm, từ tương đối trong chữ fact) không đổi và không được đụng tới.

Chưa làm / điểm yếu còn lại:

- ~~**Khoá lặp ở một phần tử trong mảng** làm mất cả cửa sổ~~ → đã làm, xem "Cứu từng phần tử" bên dưới. Còn lại: fact của phần tử hỏng mất hẳn dù nội dung còn nguyên (xem "Chưa làm" trong mục đó).
- ~~**Hai fact trong cùng một câu trả lời nói lại nhau** (bản ngắn + bản dài): luật hiện chỉ so với `KNOWN FACTS`.~~ → đã làm, xem "Hai fact cùng một câu trả lời" bên dưới.
- **Đường `Inserted::Existing` (trùng nguyên văn) không bù ngày**: fact cũ chưa có ngày, bản lặp có `when` thì ngày mất. Chỉ đường "diễn đạt lại" mới bù. Thuộc phần xử lý ngày (việc riêng).
- Không bắt được: đổi **một từ thường không phải tên và không ai liệt kê làm entity** trong câu dài ("prefers dark mode" → "prefers light mode"). Có test ghi nhận giới hạn này.
- Chưa đo trên `gemma-4-e4b`.

## Cứu từng phần tử: khoá lặp trong mảng

Kiểu hỏng còn lại sau bản sửa dấu đóng: một phần tử của mảng bắt đầu khoá hai lần, `{"text{"text":"Lan đang nghỉ phép…","kind":"world",…}`. 2/140 lần gọi (1,4%); không phải JSON dù bù dấu đóng thế nào, nên cả cửa sổ mất và `failed_chunks` +1. Hai câu trả lời thật nằm nguyên văn trong test của [`extract.rs`](../../src/memory/engine/facts/extract.rs): `GARBLED_LAST` (`after-runs-v1-first-rule/run-with_vectors-3`: 4 fact + 1 hỏng ở cuối) và `GARBLED_TWICE` (`after-runs/run-with_vectors-5`: 3 fact + **hai phần tử hỏng liền nhau**; "hai lần trong cùng một câu trả lời" là hai phần tử, không phải hai lần gọi).

Thay đổi: `salvage_elements`, chỉ chạy khi parse chặt **và** bù dấu đóng đều thất bại. Mỗi phần tử được parse riêng bằng serde; phần tử không parse được thì bỏ, phần tử kế tiếp bắt đầu ở `},{"` đầu tiên không nằm trong một `[…]` của chính phần tử hỏng (entity dạng `[{"text":"A"},{"text":"B"}]` đầy `},{"`). Neo ở `{"facts":[` đầu câu trả lời (hoặc mảng trần), không tìm `"facts"` ở giữa.

**Không quét độ sâu 1 có theo dõi chuỗi như đề xuất ban đầu.** Khoá lặp mang thừa đúng một dấu nháy, nên bộ quét theo dõi chuỗi đọc ngược mọi chuỗi phía sau nó: ngoặc thật bị coi là nằm trong chuỗi, phần tử hỏng không bao giờ "đóng". Với hai câu trả lời thật (hỏng ở cuối) kết quả vẫn đúng, nhưng phần tử hỏng ở giữa kéo theo mọi fact đứng sau: mô phỏng Python trên phần tử hỏng thật, `[A, hỏng, B]` chỉ ra A. Test `the_facts_behind_a_garbled_element_survive_it` ghim ca này (hỏng đầu / giữa / hai liền nhau / có xuống dòng / mảng trần).

**`failed_chunks` đọc thế nào.** Câu trả lời đọc được ≥ 1 phần tử và bỏ ≥ 1 phần tử **không** là chunk hỏng: `failed_chunks` vẫn là "chunk không cho ra phần tử nào đọc được" (gọi model lỗi, không có JSON, mọi phần tử hỏng). Phần tử bị bỏ được báo bằng `tracing::warn!` `[facts] extraction reply partly garbled` kèm `kept` và `dropped`; không thêm trường vào `RetainReport`.

Luật cũ giữ nguyên, có test: `{"triplets":…}` vẫn lỗi (kể cả khi có phần tử hỏng); không phần tử nào đọc được thì vẫn lỗi (không biến "hỏng hết" thành "không có gì đáng nhớ"); câu trả lời bị cắt không được cứu — mảng phải được đóng sau phần tử cuối (`}` rồi `]`), nên cắt giữa chuỗi, giữa phần tử hỏng, hay ngay sau một phần tử đều vẫn là lỗi và không bao giờ lưu nửa câu; ba kiểu dấu đóng thật + `} }` vẫn xanh. Phần tử parse được nhưng không phải fact (`7`, thiếu `text`) vẫn do `read_fact` bỏ như trước và không tính vào `dropped`.

Test: 8 test mới (28 trong `extract`), `cargo test --lib memory::engine` 169 pass, 2 ignored (harness model thật). Không đo lại bằng harness: 1,4%/lần gọi nghĩa là ~10% số lần chạy (7 lần gọi) mới chạm kiểu này, một đợt 70 lần gọi không phân biệt được; bằng chứng là test trên hai câu trả lời thật.

Chưa làm:

- **Fact của phần tử hỏng mất hẳn dù nội dung còn nguyên.** Trong cả hai mẫu, phần tử hỏng chỉ thừa đoạn `{"text` sau khoá (`{"text{"text":"…` → `{"text":"…`): sửa được, nhưng là đoán ý model nên để riêng; nếu làm thì thử sửa trước khi bỏ. Mẫu thật mất 1/5 và 2/5 fact của cửa sổ.
- **Không đếm được số lần cứu.** `RetainReport` không có trường, harness `real_extractor_*` không in, nên đo sống vẫn chỉ thấy "không còn câu trả lời hỏng". Muốn đếm: thêm `dropped_elements` vào `RetainReport` ([`retain.rs`](../../src/memory/engine/facts/retain.rs)) và in ở [`eval.rs`](../../src/memory/engine/eval.rs); `parse_facts` hiện chỉ log.
- **Câu trả lời bị cắt giữa chuỗi vẫn mất cả cửa sổ** dù các fact đứng trước đã hoàn chỉnh (luật cũ, test cũ; không đổi ở đây).

## Hai fact cùng một câu trả lời

Chỗ hở còn lại ở "Sau khi sửa": trong cửa sổ "sửa lỗi đăng nhập" `gemma-4-e2b` hay trả hai fact cùng một câu trả lời — dòng ngắn ("Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.") và bản đầy đủ ("Hôm qua, … xóa cookie cũ …, mất khoảng hai tiếng.") — và `restated_by` chỉ so với `KNOWN FACTS`, không so các fact anh em, nên cả hai được lưu.

Thay đổi ([`restated.rs`](../../src/memory/engine/facts/restated.rs) `without_restated_siblings`, gọi trong [`retain.rs`](../../src/memory/engine/facts/retain.rs) ngay sau `parse_facts`, trước embed và lưu):

- **So các `Extracted` với nhau, không đẩy fact vừa lưu vào danh sách `KNOWN FACTS`.** Hai lý do. (1) Bản ngắn đứng trước sẽ được lưu trước, rồi bản dài mới đến: `restated_by(vừa_lưu, dài)` không bắt được (bản dài *thêm* ý), và muốn bỏ bản ngắn phải ẩn/xoá một fact đã lưu. So trước khi lưu thì "bản ngắn bỏ, bản đầy đủ giữ, bất kể ai nói trước". (2) `replaces` đánh số theo `KNOWN FACTS` của chunk; chèn fact vừa lưu vào đó làm lệch số. Bonus: fact bị bỏ không tốn một lần embed. `restated_by` giữ nguyên chữ ký; phần lõi tách thành `restated_share` trên một view `Claim` chung cho `StoredFact` và `Extracted`.
- **Cùng các điều kiện** (số/ngày, phủ định, tên, entity hai phía, `when`, `replaces`) **cộng ba điều khác**: (a) *mọi* từ của fact bị bỏ phải có ở fact kia (`SIBLING_SHARE = 1.0`, thay vì 0.8) — trong một câu trả lời không có câu chữ đã lưu nào để model diễn đạt lại, và đổi một từ thường ("buổi tối" → "buổi sáng", 6/7 từ) là điều không gì khác bắt được; (b) fact có `replaces` luôn ở lại và được xét trước, nên bản sao không `replaces` của nó mới là bản bị bỏ (`Inserted::Existing` bỏ qua `replaces`); (c) fact **có ngày** không bị bỏ vào fact không có ngày (nhánh fact đã lưu thì `date_if_missing` chuyển ngày sang; giữa anh em không ai làm việc đó, và "không đổi xử lý ngày"). Bản ngắn không ngày bỏ vào bản dài có ngày thì được (đúng ca đo).
- **Thứ tự xét**: dài nhất trước (số từ, rồi thứ tự nói), mỗi fact so với những fact *đã chốt giữ*. Một fact chỉ bị bỏ vì một anh em nói nhiều hơn hoặc bằng, và anh em đó chắc chắn ở lại (không có chuỗi A→B→C bỏ hết). Bằng nhau thì fact nói trước ở lại. Fact còn lại giữ nguyên thứ tự gốc.
- **Bộ đếm: trường mới `RetainReport::folded`, không dùng `reinforced`.** `reinforced` = một fact *đã lưu* vừa được nhắc lại (`touch`, `proof_count + 1`, ngày đưa lên). Fact bị bỏ vì anh em trong cùng câu trả lời không chạm vào fact đã lưu nào, và một câu trả lời là một lần nhắc: tính vào `reinforced` thì `reinforced` không còn bằng tổng `proof_count` đã tăng, và hai bản của cùng câu trả lời thành "bằng chứng" thêm cho nhau. Đẳng thức mới: `extracted = stored + reinforced + folded`. Hệ quả có chủ ý: bản ngắn và bản dài *cùng* nhắc lại một fact đã lưu, trước đây `touch` hai lần (`proof_count` +2 cho một cửa sổ), nay một lần (test `a_sibling_that_stays_is_still_judged_against_what_is_stored`). Và hai fact **trùng nguyên văn** trong một câu trả lời, trước đây tăng `proof_count` qua `Inserted::Existing`, nay gộp thành một.
- Harness `real_extractor_*` (`eval.rs`, của phiên khác nên không đụng) chưa in `folded`; số dưới đây lấy `folded = extracted − stored − reinforced` (`failed`/lỗi lưu đều 0).

Test: 14 test đơn vị mới trong `restated.rs` (ngắn+dài bị bỏ theo cả hai thứ tự với nguyên văn log; chữ/ngày/entity bản dài không đổi; bản sao thiếu số/vế; ba fact lồng nhau; hai fact cùng nghĩa giữ fact nói trước; thứ tự sống sót; hai fact khác nhau chung chủ ngữ — 4 cặp, cả hai thứ tự — giữ cả hai; lệch một tên/số/phủ định giữ cả hai; đổi một từ thường giữ cả hai; entity lệch hai phía; khác ngày; fact có ngày không bỏ vào fact không ngày; `replaces`; 0–1 fact; mảnh vụn) + 3 test `retain` (ngắn+dài cùng câu trả lời lưu một, `proof_count` 1, giữ ngày; hai fact lệch một số lưu cả hai; anh em ở lại vẫn bị so với fact đã lưu và `touch` một lần). `cargo test --lib memory::engine`: 169 pass, 2 ignored; `memory::` 410 pass. **Một test cũ phải đổi fixture**: `related_facts_are_linked_by_meaning_and_by_date` dùng "Sen lives in Hanoi." + "Sen lives in Hanoi and moved there." trong một câu trả lời — đúng cặp ngắn ⊂ dài mà luật mới gộp; đổi fact thứ hai thành "Sen moved to Hanoi and lives there." (cùng vector, cùng ngày, không chứa fact đầu), assert giữ nguyên.

Đo trên log cũ (bản Python của luật, không entity, không kiểm `when`/`replaces` ngoài ngày; `replay.py` trong thư mục ghi chú): 39 file log (các đợt "final", "earlier", "after-runs" và "after-runs-v1"), 565 fact trích → 20 ứng viên bị bỏ; **cả 20 là cùng một câu ngắn** ("Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.") bỏ vào một bản dài "Hôm qua, … mất khoảng hai tiếng" (kể cả bản tách câu, bản "(2026-10-15)" và bản "… vào hôm qua"), soát tay đều là nhắc lại. Không fact nào khác bị bỏ.

Đo sống (cùng model `gemma-4-e2b` 4-bit, `sen-mlx 0.1.1` scratch trên `127.0.0.1:38947`, `HOME`/`SENCLAW_HOME`/cwd tạm, daemon thật 18788/18789 không đụng, runtime scratch đã dừng; 5 keyword-only + 5 embedder băm từ, 70 lần gọi; số thô ở `side-session-notes/local_b75dc744-…/sibling-runs/`). Model không tất định và đợt này là một lần rút khác với đợt "sau" ở trên, nên chỉ đủ thấy hướng:

| | keyword trước (5) | keyword sau (5) | vector trước (5) | vector sau (5) |
|---|---|---|---|---|
| trích / lưu / tăng chứng / `folded`, TB | 14.6 / 10.0 / 4.6 / – | 15.6 / 10.6 / 4.8 / 0.2 | 13.4 / 10.8 / 2.6 / – | 13.4 / 10.6 / 2.6 / 0.2 |
| fact còn sống cuối lần chạy (fixture: 8) | 9.0 | 9.8 | 10.0 | 9.6 |
| "sửa lỗi đăng nhập" còn sống (TB) | 1.2 | 1.6 | 2.0 | 1.4 |
| cặp sống nói lại nhau (TB, bản Python cũ) | 0.2 | 0.6 | 1.2 | 0.0 |
| câu đúng khi bỏ dấu (TB trên 7) | 6.6 | 6.8 | 6.0 | 7.0 |
| `LEAKED` / JSON hỏng | 0/5 / 0/35 | 0/5 / 0/35 | 1/5 / 1/35 | 0/5 / 0/35 |

("Trước" = 10 lần chạy của mục "Sau khi sửa", "sau" = đợt này; cột trước dùng lại số của mục đó.)

Đọc kết quả, thẳng thắn:

1. **Luật chạy đúng chỗ nó nên chạy và không chỗ nào khác.** `folded = 1` ở đúng hai lần chạy (keyword-1, vector-5), đúng hai lần mà bản Python của luật dự đoán trên log (`replay.py`: 2 ứng viên trong 145 fact), và cả hai là cặp ngắn ⊂ dài. Không fact thật nào bị bỏ.
2. **Số liệu cuối kho không chứng minh được lợi ích** (login-fix keyword 1.2 → 1.6, vector 2.0 → 1.4; cặp nói lại 0.2 → 0.6 và 1.2 → 0.0): mẫu 5 lần chạy, model đổi cách nói giữa hai đợt, và phần dư không còn là kiểu này. Chỉ 2/10 lần chạy có cặp ngắn⊂dài; ở 4/10 lần chạy bản ngắn là **một khẳng định khác** ("Người dùng *đang làm việc sửa* lỗi đăng nhập…", "đã gặp vấn đề với lỗi đăng nhập…": 0.64–0.75 từ có trong bản dài), nên giữ cả hai là đúng — và cũng dưới 0.8, nên nới ngưỡng xuống 0.8 cũng không đổi gì ở mẫu này. **Chặt hơn (1.0) không làm mất ca nào trong mẫu**: không cặp nào có chia sẻ trong [0.8, 1.0).
3. **Phần dư còn lại, đã soát tay, không thuộc luật này**: (a) keyword-4 — hai bản dài sống cạnh nhau vì bản nhắc lại tự ghi "(2026-10-15)" *trong chữ* và `restated_by` chặn mọi số/ngày mới trong chữ (luật ngày không đụng tới theo yêu cầu); (b) keyword-1 — model gộp hai fact đã biết thành một ("Lan phụ trách dự án Falcon và đang nghỉ phép…"), bản gộp *thêm vế* so với từng fact lưu nên theo luật là fact mới; (c) bản "đang làm việc sửa" ở mục 2.
4. Độ trễ model, `replaces` (9/10 lần nêu, 9/9 chấp nhận), JSON hỏng (0/70): không đổi.

Chưa làm / điểm yếu của phần này:

- **Giữa các chunk của một lần `retain`** vẫn không so nhau: mỗi chunk có câu trả lời riêng và `KNOWN FACTS` lấy *trước* khi chunk nào được lưu. Cửa sổ của eval chỉ có 1 chunk nên chưa đo.
- **Guard entity phía fact dài giữ nguyên như fact đã lưu**: nếu bản dài liệt kê một entity mà bản ngắn không có từ nào ("cookie"), cả hai được lưu (trùng lặp, không mất fact). `gemma-4-e2b` chỉ liệt kê `["user"]` cho bản dài nên chưa gặp; model khác (e4b, chưa đo) có thể gặp. Với `SIBLING_SHARE = 1.0` guard này gần như thừa (đổi một từ làm share < 1), nên bỏ nó cho cặp anh em là hướng làm tiếp nếu số liệu e4b cho thấy bị lỡ — chưa làm vì chưa có bằng chứng.
- Túi từ không thấy thứ tự: "thích React hơn Vue" và "thích Vue hơn React" có cùng tập từ (tên nằm trong `identity` ở cả hai phía), nên sẽ gộp. Hiếm; đã đúng với luật fact đã lưu từ trước.
- Harness chưa in `folded` (xem trên).
