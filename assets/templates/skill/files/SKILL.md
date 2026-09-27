---
# Quoted on purpose: these values come from the command line, and a colon or a
# quote in an ordinary description ("Trợ lý: quản lý kho") breaks an unquoted
# YAML scalar — taking `name` down with it, since the whole block fails to parse.
name: "{{id}}"
description: "{{description}}"
version: 0.1.0
when-to-use: Khi người dùng ... (mô tả tình huống cụ thể, không phải chủ đề chung chung)
triggers:
  - "{{id}}"
  - "{{title_name}}"
# Chỉ khai báo khi skill thật sự cần một tool cụ thể. Danh sách này là
# whitelist: khai báo sai tên tool thì agent mất tool đó chứ không báo lỗi.
# allowed-tools:
#   - mcp__senclaw-memory__memory_search
---

# {{title_name}}

{{description}}

## Khi nào dùng

- Người dùng nói "..." hoặc hỏi "..."
- Có sẵn dữ liệu ... và cần ...

## Khi nào KHÔNG dùng

- ... (ranh giới này quan trọng ngang phần trên: một skill mô tả quá rộng sẽ bị
  gọi nhầm ở mọi câu hỏi, và người dùng sẽ thấy agent chậm đi mà không hiểu vì
  sao)

## Cách làm

1. **Xác định đầu vào.** ...
2. **Thực hiện.** ...
3. **Trả kết quả.** ... — nói rõ định dạng agent nên trả về.

## Ví dụ

**Người dùng:** ...

**Agent làm:** ...

**Trả về:**

```
...
```

## Lưu ý

- Tên tool MCP của SenClaw luôn có dạng đầy đủ
  `mcp__senclaw-<domain>__<prefix>_<verb>` — không có dạng rút gọn nào phân giải
  được. Hai ngoại lệ prefix: `senclaw-cognitive` dùng `cog_*`, `senclaw-sandbox`
  dùng `sbx_*`.
- Viết hướng dẫn cho agent, không phải cho người đọc: câu mệnh lệnh, cụ thể, và
  nói rõ cái gì *không* nên làm.
