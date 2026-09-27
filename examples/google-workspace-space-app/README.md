# Google Workspace Space App

An installable **server** Space App: a single Next.js process serves the web UI,
the MCP route (`/mcp`), a `/health` probe, and a same-origin proxy to the
SenClaw daemon (`/api/space/*`). SenClaw launches it (manifest `runtime.start`)
with an assigned `PORT`, then auto-registers its MCP and installs its bundled
skill — **no manual "Register MCP" step**.

## Architecture

```
SenClaw daemon ──launches `npm start`/`node server.js` (PORT=4310)──▶ Next.js app
   │  ▲                                                                  │ serves:
   │  │  auto-register MCP (http://127.0.0.1:4310/mcp)                   │  • / (UI iframe)
   │  │  install bundled skill (read-only)                              │  • /mcp (MCP route)
   │  └──────────── /api/space/* proxy ◀────────────────────────────────┤  • /health
   └─ stamps runtime.url into the manifest (iframe loads it)             │  • /api/space/* → daemon
```

- **MCP is a Next.js Route Handler** ([`app/mcp/route.js`](app/mcp/route.js)) using
  the MCP SDK's Web Standard transport — no separate server process.
- **Skill** ([`skills/google-workspace/`](skills/google-workspace)) is installed
  with the app and removed on uninstall; it is read-only in the Skills panel.

## Develop

```bash
npm install
npm run dev      # Next dev server
```

Register the running dev app with a daemon (it runs `npm start` in place):

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' \
  -d '{"path":"'"$PWD"'"}'
```

## Build & pack a ZIP

```bash
npm run build       # next build (output: 'standalone')
npm run pack:zip    # build + assemble + zip → google-workspace-space-app.zip
```

`pack:zip` bundles the Next.js **standalone** output (`server.js` + a minimal
`node_modules`), the static assets, `public/`, the `skills/` folder, and a
manifest whose `runtime.start` is `node server.js` — a self-contained (~16 MB)
ZIP that needs no `npm install` on the target.

## Install the ZIP

`Space → Apps → Cài từ ZIP` (or `Settings → Space Apps`), or:

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/install-zip \
  -F "file=@google-workspace-space-app.zip"
```

SenClaw extracts it, launches the standalone server on an assigned port, waits
for `/health`, auto-registers the MCP, and installs the bundled skill. Uninstall
stops the process, unregisters the MCP, and removes the skill.

## Tools (MCP server `google-workspace-mcp`)

- `gworkspace_get_settings` — read saved settings (**đọc cài đặt**)
- `gworkspace_set_settings` — merge-update settings (**ghi cài đặt**)
- `gworkspace_sync` — sync Gmail / Calendar via a Google OAuth token

Settings are stored in the SenClaw config KV (`google-workspace-settings`),
shared with the in-app Settings dialog.
