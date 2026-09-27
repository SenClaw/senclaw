# Python Demo — Space App

Space App tối giản viết bằng Python, dùng để đối chiếu khi viết app mới.
Bản Node tương đương: [`../../../senclaw-app-sdk/examples/space-app-node-demo`](../../../senclaw-app-sdk/examples/space-app-node-demo).

```bash
# Chạy tay (dev)
PYTHONPATH=../.. SENCLAW_SPACE_APP_ID=python-demo PORT=4810 python main.py

# Cài vào daemon đang chạy
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' \
  -d "{\"path\": \"$(pwd)\"}"
```

Manifest khai đủ 4 thứ mới:

| Khai | Nghĩa |
|---|---|
| `runtime.mode: "session"` | Không chạy lúc daemon khởi động. Chạy khi mở app hoặc agent gọi tool, dừng sau `idleTimeoutSecs` giây rảnh |
| `runtime.runner: "python"` | Daemon tạo `.venv` trong thư mục app và cài `requirements.txt` vào đó |
| `requires.python: ">=3.10"` | Kiểm tra lúc cài **và** trước mỗi lần chạy; thiếu thì báo lý do, không chạy |
| `sandbox` | Bật sandbox ngay từ lúc cài, không đợi người dùng vào Plugins bật tay |

Xem thêm: [`docs/space-app-lifecycle.md`](../../../../docs/space-app-lifecycle.md).
