# WeChat Channel Binding Guide

> **Note**: WeChat binding requires the **user to scan a QR code manually**. The agent cannot perform this step for the user. This guide only explains the steps for the user to run in terminal.

---

## Prerequisites

WeChat channel uses **iLink Bot API** (`ilinkai.weixin.qq.com`) based on the WeCom iLink protocol. You **do not need to apply for App ID/App Secret in advance**. Credentials are issued and saved automatically by the server after QR login.

---

## Method 1: Primary Account (`.env` configuration)

For single-account setup, binding to `agents/main/`.

**Step 1: Configure `.env`**

```bash
# .env
WECHAT_ENABLED=true

# Optional: bind to another folder (default: main)
# WECHAT_AGENT_FOLDER=main
```

**Step 2: Start and scan QR code**

```bash
senclaw start
```

After startup, terminal shows a QR code. Scan it in WeChat to finish login. Credentials are saved to:

```
~/.senclaw/wechat/accounts/default.json
```

On future restarts, you usually do not need to scan again (long-lived credentials). If session expires, QR code appears again on restart.

---

## Method 2: Additional Accounts (CLI)

For binding multiple WeChat accounts to different agent folders.

> **Web UI does not support adding WeChat accounts** (QR scanning must be done in terminal). Web UI supports delete only.

### Add

```bash
senclaw channel wechat add --group <folder> [--name <name>]
```

| Parameter | Required | Description |
|------|------|------|
| `--group` | ✓ | Target agent folder (lowercase letters/numbers/hyphens, e.g. `alice`) |
| `--name` | | Optional display name (default is folder name) |

Restart `senclaw` after adding. Terminal will show a QR code for that account, and credentials will be saved to:

```
~/.senclaw/wechat/accounts/<folder>.json
```

**Example:**

```bash
# Add a WeChat account for folder alice
senclaw channel wechat add --group alice --name "Alice WeChat"

# Restart and scan the QR code shown in terminal
senclaw start
```

After the bot receives the **first message**, system auto-migrates `wx:pending:<folder>` to the real JID (`wx:user:<userId>`) with no extra action.

### List

```bash
senclaw channel wechat list     # WeChat account list
senclaw channel list            # all channel types summary
```

### Remove

```bash
senclaw channel wechat remove --group <folder>
```

This also removes:
- `~/.senclaw/wechat/accounts/<folder>.json` (credentials)
- `~/.senclaw/wechat/sync-buf-<folder>.bin` (message cursor)
- `~/.senclaw/wechat/context-tokens-<folder>.json` (conversation token cache)
- related group binding in `config.json`

Restart `senclaw` after deletion to apply changes.

> **Note**: `remove` only applies to Method 2 (CLI-added accounts). For the primary account in Method 1 (`.env`), set `WECHAT_ENABLED=false` in `.env`, delete `~/.senclaw/wechat/accounts/default.json`, then restart.

---

## Pending Auto-Binding

WeChat user IDs are not known in advance, so binding flow is:

1. After CLI add, system records `wx:pending:<folder>`.
2. When the bot receives the **first message**, it auto-migrates to real JID (`wx:user:<userId>`).
3. Agent immediately processes that same message after migration (no resend needed).

---

## Trigger Behavior

WeChat iLink bot currently supports **1:1 direct messages only**. Every message triggers the agent; no @mention is required.

There are **no buttons** in the iLink protocol. When the agent offers choices
(a permission prompt, a question), they arrive as a numbered list and the user
replies with the number; that choice is fed back as if the user had typed the
option's label.

---

## Images and files

Both directions work and both are encrypted on WeChat's CDN (AES-128-ECB with
a per-message key), which the daemon handles transparently.

- **Inbound**: images, files and videos a user sends are downloaded, decrypted
  and attached to the turn, so a vision model sees the picture and the agent
  can read the document. Voice notes arrive as the server's transcript.
- **Outbound**: `send_file` encrypts the file, uploads it to the CDN and sends
  it as an image, video or file item, chosen by the file's type. The cap is the
  same 32 MB as a chat upload.

A wrong decryption key yields garbage rather than an error, so an inbound
picture is only attached once its bytes actually start with a known image
signature; anything else is logged and skipped, and the message still reaches
the agent as text.

---

## FAQ

**Q: No QR code appears after startup**  
-> Check `WECHAT_ENABLED=true` in `.env` (Method 1), or ensure `wechatAccounts` exists in `config.json` (Method 2). If credential file already exists, previous login is still valid and re-scan is not needed.

**Q: QR code expired after scanning**  
-> This is normal. System auto-refreshes QR code (up to 3 times). If it keeps expiring, check network connectivity and restart.

**Q: Messages arrive but agent does not reply**  
-> Check logs for `WeChat pending binding completed` (first run of Method 2) or `WeChatChannel connected`. If logs mention missing `context_token`, that is an old-version bug; upgrade and restart.

**Q: What to do when session expires?**  
-> If logs show `session expired, re-scan required`, remove credentials and restart:
```bash
# Method 1 primary account
rm ~/.senclaw/wechat/accounts/default.json

# Method 2 additional account (alice example)
senclaw channel wechat remove --group alice
senclaw channel wechat add --group alice --name "Alice WeChat"
```

**Q: Can one WeChat account bind to multiple folders?**  
-> No. Each WeChat account (iLink bot) can bind to only one folder (one agent instance).
