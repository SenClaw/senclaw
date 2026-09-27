# Telegram Channel Binding Guide

## Prerequisites

In Telegram, open [@BotFather](https://t.me/BotFather) and send `/newbot` to create a bot, then copy the **Bot Token**.

You do **not** need to look up anyone's Telegram user id. A chat identifies itself by messaging the bot; you approve it by code (see *Pairing* below).

---

## How access works

A Telegram chat is answered only if it has a **binding**. Everything else gets a
pairing code and nothing more:

1. Register a Telegram channel and an agent (Web UI → Settings → Channels / Agents, or `.env` for the primary bot).
2. The user opens Telegram and sends any message to the bot.
3. The bot replies with an **8-character code** and does not process the message.
4. You approve that code. The chat is bound and can talk from then on.

Nothing is bound before step 4. A message from an unknown chat can never grant
itself access — this is the whole point of the flow.

---

## Method 1: Primary Bot (`.env` configuration)

Use this for the first / primary bot. It binds to `agents/main/` by default.

```bash
# .env
TELEGRAM_BOT_TOKEN=123456:ABC-your-token

# Optional: bind to a different folder (default: main)
# TELEGRAM_AGENT_FOLDER=main
```

Restart `senclaw` after editing to apply changes, then pair as above.

---

## Method 2: Additional Bots (Web UI)

Use this to bind second/third bots to different agent folders. Config is saved
to `~/.senclaw/config.json` and takes effect immediately without restart.

Open Settings → Channels → **Add Channel**, set Platform to `Telegram`.

| Field | Required | Description |
|------|------|------|
| Channel Name | ✓ | Shown in the UI |
| Bot Token | | Dedicated bot token; leave empty to use `TELEGRAM_BOT_TOKEN` from `.env` |
| Chat Type | ✓ | `User` (DM) or `Group` |
| Require @mention | | In groups, only answer when mentioned |

Then create an agent (Settings → Agents → **Add Agent**) and attach this
channel to it. That leaves a binding waiting for its chat — which the first
**approved** pairing fills in.

> An empty Bot Token means "use the default bot from `.env`", and only that
> bot. It does not mean "any bot": a message arriving through an unrelated
> token is not matched to this channel.

---

## Pairing: approving a chat

**Web UI** — Settings → Channels → *Pairing — chats đang chờ duyệt*. Each
waiting chat shows its code, the sender's name, and whether it is a DM or a
group. Approve or reject there, or paste a code the user sent you.

**CLI**

```bash
senclaw pairing list                 # who is waiting
senclaw pairing approve K7M2PQ4R     # let them in (dashes/case ignored)
senclaw pairing reject 12            # turn a request away, by id
```

Notes:

- Codes **expire after 1 hour**. An expired code is refused; the user sends the bot another message to get a new one.
- Repeat messages reuse the same code — a user who says "hello" three times is one request, not three.
- Approving a **group** lets in every member of that group, not one person. The UI says so before you click.
- The first approval fills the channel's waiting binding. Later approvals on the same channel create their own binding onto the same agent, so several chats can share one bot.
- Rejecting does not blacklist anyone; they can ask again. The default state is "not bound", which is already a refusal.

---

## Multiple Bots in One Folder

Two bots can bind to the same folder (same agent). In that case:

- Each bot polls Telegram independently.
- All messages are routed to the **same agent instance** and processed serially.
- Conversation history from both sides is **shared** (like a merged chat context).

Recommendation: unless you explicitly need a backup account, bind **one bot per folder** to avoid serial blocking and context pollution.
