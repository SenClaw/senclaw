# Feishu Channel Binding Guide

## Prerequisites

### 1. Create a Feishu custom app and enable bot capability

1. Open [Feishu Open Platform](https://open.feishu.cn/app) -> **Create enterprise custom app**
2. In the app, go to left menu **Add capabilities** -> choose **Bot**
3. Record **App ID** and **App Secret** (Credentials & Basic Info page)

---

### 2. Configure event subscription and card callbacks

In the app, go to **Events & Callbacks**:

**Event settings:**
- Subscription type: **Receive events via long connection**
- Click Add event, search and add:
  - `im.message.receive_v1` (receive messages)
  - `im.chat.member.bot.added_v1` (optional, bot added to group)

**Card callback settings:**
- Callback type: **Receive callbacks via long connection**
- Add callback type: `card.action.trigger` (used by permission approval interactive buttons)

---

### 3. Batch-add permissions

In the app, go to **Permission Management** -> click **Batch Add** (JSON import):

```json
{
  "scopes": {
    "tenant": [
      "im:message.p2p_msg:readonly",
      "contact:contact.base:readonly",
      "contact:user.base:readonly",
      "docx:document",
      "docx:document.block:convert",
      "docx:document:create",
      "docx:document:readonly",
      "docx:document:write_only",
      "im:chat:readonly",
      "im:message",
      "im:message.group_at_msg:readonly",
      "im:message:readonly",
      "im:message:send_as_bot",
      "im:resource",
      "wiki:member:create",
      "wiki:member:retrieve",
      "wiki:member:update",
      "wiki:node:copy",
      "wiki:node:create",
      "wiki:node:move",
      "wiki:node:read",
      "wiki:node:retrieve",
      "wiki:node:update",
      "wiki:setting:read",
      "wiki:setting:write_only",
      "wiki:space:read",
      "wiki:space:retrieve",
      "wiki:space:write_only",
      "wiki:wiki",
      "wiki:wiki:readonly"
    ],
    "user": [
      "contact:user.employee_id:readonly",
      "im:chat",
      "im:chat.managers:write_only"
    ]
  }
}

```

> `contact:user.base:readonly` is used to resolve sender real names. Skip it if real-name display is not needed.

---

### 4. Publish the app

Go to **Version Management & Release** -> create a version -> submit release (for internal enterprise apps, review is usually not required).

> ⚠️ After changing event subscriptions or permissions, you **must publish a new version** for changes to take effect.

---

## Method 1: Primary App (`.env` configuration)

Use this for the first / primary Feishu app by writing directly to `.env`:

```bash
# .env
FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

# Optional: `lark` (international) or custom domain, default is `feishu`
# FEISHU_DOMAIN=feishu
```

Restart `senclaw` after editing to apply changes.

---

## Method 2: Additional Apps (Web UI or CLI)

Use this to bind second/third apps to different agent folders. Config is saved to `~/.senclaw/config.json` and takes effect immediately without restart.

### Web UI Configuration

Open Settings -> Agents -> **Add Agent**, then choose `Feishu` as Channel:

| Field | Required | Description |
|------|------|------|
| Display Name | ✓ | Agent name shown in UI |
| Agent ID | ✓ | Target agent folder (lowercase letters/numbers/hyphens) |
| App ID | ✓ | Feishu app App ID |
| App Secret | ✓ | Feishu app App Secret |
| Chat JID | | Optional; auto-bound after first message if empty |

**Chat JID behavior:**
- Leave empty -> stored as `feishu:pending:{appId}`; bot auto-migrates to real JID (group or DM) after first message.
- Fill manually -> format `feishu:group:oc_xxx` (group) or `feishu:user:ou_xxx` (DM).

> Each pending binding can migrate to **only one** JID (wherever the first message comes from). To support both group and DM, add two bindings.

### CLI Configuration

```bash
# Add (leave JID empty for auto-binding)
senclaw channel feishu add \
  --app-id cli_xxxxxxxxxxxxxxxx \
  --app-secret xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx \
  --group mybot \
  --name "My Feishu Assistant"

# Add (specify known JID, skip pending flow)
senclaw channel feishu add \
  --app-id cli_xxxxxxxxxxxxxxxx \
  --app-secret xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx \
  --group mybot \
  --jid feishu:group:oc_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

# International version (Lark)
senclaw channel feishu add \
  --app-id cli_xxxxxxxxxxxxxxxx \
  --app-secret xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx \
  --group mybot \
  --domain lark

# List
senclaw channel feishu list
senclaw channel list            # all channel types summary

# Remove (also removes related group binding)
senclaw channel feishu remove --app-id cli_xxxxxxxxxxxxxxxx
```

| Parameter | Required | Description |
|------|------|------|
| `--app-id` | ✓ | Feishu app App ID |
| `--app-secret` | ✓ | Feishu app App Secret |
| `--group` | ✓ | Target agent folder (lowercase letters/numbers/hyphens) |
| `--name` | | Optional display name (default same as folder) |
| `--jid` | | Optional Chat JID (auto-binding pending if empty) |
| `--domain` | | `feishu` (default) or `lark` (international) |

> CLI add takes effect after restart; Web UI add is instant. Remove operations sync to `config.json` in both methods.

> **Note**: `remove` only applies to Method 2. For the primary app in Method 1 (`.env`), manually clear related fields in `.env` and restart.

---

## Images and files

Inbound images, files, audio and video are downloaded through
`GET /open-apis/im/v1/messages/{message_id}/resources/{key}` and attached to
the turn, so a vision model sees the picture and the agent can read the
document. Images embedded in a rich-text `post` are picked up too.

This needs the **`im:resource`** scope in the permission list above (it is
already in the batch-add JSON). Without it Feishu answers the download with a
403 JSON body; the daemon logs that and delivers the message's text rather
than attaching an error page as if it were a picture.

Outbound `send_file` uploads through `/open-apis/im/v1/files` and sends a
`file` message, with any caption following as a separate text message.

---

## Trigger Behavior

| Scenario | Default behavior |
|------|---------|
| Bot in DM | Every message triggers the agent |
| Group (`requiresTrigger = true`) | Must @mention the bot |
| Group (`requiresTrigger = false`) | Every message triggers the agent |

You can change `requiresTrigger` in Agent settings in Web UI.

---

## FAQ

**Q: Sending messages gets no response (no logs)**  
-> Check that event subscription is set to long connection and `im.message.receive_v1` is added. Publish a new app version, then test again.

**Q: There are logs when bot joins group, but no logs when sending messages**  
-> `im.message.receive_v1` is not subscribed, or it was added but not published in a new app version.

**Q: Multiple bots in one group; @new bot but old bot replies**  
-> Bots from different apps should be bound to **different groups**. Do not add two `senclaw` bots to the same group.

**Q: DM does not work**  
-> Ensure permission `im:message.p2p_msg:readonly` exists and the app has been published within the enterprise tenant.
