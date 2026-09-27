# Node Demo — Space App

Space App tối giản viết bằng Node, không phụ thuộc thư viện ngoài.
Bản Python tương đương: [`../../../senclaw-app-sdk-python/examples/space-app-python-demo`](../../../senclaw-app-sdk-python/examples/space-app-python-demo).

```bash
# Chạy tay (dev)
SENCLAW_SPACE_APP_ID=node-demo PORT=4820 node server.mjs

# Cài vào daemon đang chạy
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' \
  -d "{\"path\": \"$(pwd)\"}"
```

| Khai | Nghĩa |
|---|---|
| `runtime.mode: "session"` | Không chạy lúc daemon khởi động. Chạy khi mở app hoặc agent gọi tool, dừng sau `idleTimeoutSecs` giây rảnh |
| `runtime.runner: "node"` + `install` | Daemon chạy `npm install --omit=dev` một lần sau cài/update (dấu vân tay theo nội dung `package.json` + lockfile) |
| `requires.node: ">=18"` | Kiểm tra lúc cài **và** trước mỗi lần chạy |
| `sandbox` | Bật sandbox ngay từ lúc cài |

App dùng SDK TypeScript thì thêm `@senclaw/space-sdk` và
`import { bindHost, appPort, onShutdown } from '@senclaw/space-sdk/lifecycle'` —
xem [`../..`](../..).

Xem thêm: [`docs/space-app-lifecycle.md`](../../../../docs/space-app-lifecycle.md).
