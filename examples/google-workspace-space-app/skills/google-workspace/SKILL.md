---
name: google-workspace
description: Operate the Google Workspace Space App — sync Gmail/Calendar into Space and read or update the app's settings. Use when the user wants to import Google Workspace data, check or change the sync window/services, or inspect what the Google Workspace connector will pull.
version: 1.0.0
when-to-use: "sync my Gmail", "import Google Calendar", "change Google Workspace sync to 30 days", "what Google services am I syncing", "set up Google Workspace import". Installed automatically with the Google Workspace Space App; removed when the app is uninstalled.
metadata:
  installed_by_app: google-workspace
---

# Google Workspace Skill

Bundled with the **Google Workspace** Space App. It documents the MCP tools the
app exposes so the agent can drive Workspace sync and settings without the user
opening the app UI.

> This skill is installed and removed with the app. It is read-only in the Skills
> panel — edit the app instead.

## Tools (MCP server `google-workspace-mcp`)

| Tool | Purpose |
|------|---------|
| `gworkspace_get_settings` | Read saved settings (sync window, services, MCP name/port). |
| `gworkspace_set_settings` | Merge-update settings (only the fields you pass). |
| `gworkspace_sync` | Sync Gmail / Calendar into Space using a Google OAuth token. |

## How to use

1. **Check current settings** with `gworkspace_get_settings` before syncing so you
   know the active window and enabled services.
2. **Adjust if asked** — e.g. "sync 30 days" → `gworkspace_set_settings { days: 30 }`;
   "only Gmail" → `gworkspace_set_settings { services: ["gmail"] }`.
3. **Sync** with `gworkspace_sync`. A Google OAuth access token (`ya29...`) is
   required and is used only for that run. Unspecified `days`/`services` fall back
   to the saved settings.

## Notes

- Notes sync is reserved (Keep/Drive) and currently returns a pending status.
- The token is never persisted. Ask the user for a fresh token per sync run.
