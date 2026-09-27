---
name: ocr
description: Extract text from images, screenshots and scanned documents using on-device OCR (PaddleOCR + MNN). Supports Vietnamese, English, Chinese and 40+ Latin-script languages.
allowed-tools:
  - Read
  - Bash
  - Glob
  - ocr_recognize
  - ocr_batch
when-to-use: When the user provides an image file or asks to read/extract text from a picture, screenshot, scanned PDF page or photographed document.
triggers:
  - ocr
  - scan
  - extract text
  - read image
  - đọc ảnh
  - đọc chữ trong ảnh
  - quét tài liệu
  - trích xuất chữ
  - nhận dạng văn bản
metadata:
  openclaw:
    os: [darwin, linux, win32]
---

# OCR — On-device text extraction

Use the built-in `senclaw-ocr` MCP server to extract text from images entirely
on-device. No cloud calls, no Python; the model runs through PaddleOCR + MNN.

## Tools

- **`ocr_recognize(image_path, language?)`** — single image → recognized text
  and per-block bounding boxes.
- **`ocr_batch(dir, glob?, language?)`** — every matching image in a folder.
  Returns one entry per file. Capped at 64 files per call.

## Decision flow

1. **Verify the input.** Confirm the path exists and is a supported format
   (`png`, `jpg`, `jpeg`, `webp`, `bmp`, `gif`). Use `Read`/`Bash` if needed.
2. **Pick the right tool.**
   - One image → `ocr_recognize`.
   - A folder, or several files matching a pattern → `ocr_batch`.
3. **Language hint.** Pass `language` only when the user specifies one
   (e.g. `"vi"`, `"en"`, `"zh"`). Otherwise the default OCR language from
   Settings is used.
4. **Report results clearly.** Echo the recognized text verbatim. Flag any
   block with `confidence < 0.6` as low-confidence and ask whether to retry
   with another model.

## Setup hints

If the call returns *"no OCR model selected or installed"*, ask the user to
open **Settings → OCR** in the Web UI and download a catalog model — the
recommended one for Vietnamese / English is
`PP-OCRv5_mobile_latin`.

## Examples

> "Đọc chữ trong /tmp/hoadon.png"
> → `ocr_recognize({ image_path: "/tmp/hoadon.png", language: "vi" })`

> "Trích xuất chữ từ tất cả screenshot trong ~/Desktop/captures"
> → `ocr_batch({ dir: "~/Desktop/captures", glob: "*.png" })`
