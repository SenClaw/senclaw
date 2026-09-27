# Desktop app localization (English / Tiếng Việt)

The Flutter desktop app ships in English and Vietnamese. The user picks the
language in **Settings → Appearance → Language** (System / English / Tiếng
Việt); the choice persists in `SharedPreferences` under
`senclaw:app-language` and applies immediately across every screen, the tray
menu, and OS notifications.

## How it works

**The English string is the key.** There are no `intl` ARB files and no
codegen: `context.tr('Add channel')` looks `'Add channel'` up in the Vietnamese
map and returns the English string unchanged when the language is `en` or when
no translation exists. A missing translation therefore degrades to English
instead of showing a raw key — which is why the sweep could be parallelized
across the whole app without a coordination step.

| File | Role |
|---|---|
| [`lib/core/i18n/l10n.dart`](../desktop_app/lib/core/i18n/l10n.dart) | `L10n` lookup, `L10nDelegate`, `context.tr/trArgs/trPlural`, `L10n.global` |
| [`lib/core/i18n/locale_provider.dart`](../desktop_app/lib/core/i18n/locale_provider.dart) | `AppLanguage` enum, persisted `appLanguageProvider`, resolved `localeCodeProvider` |
| `lib/core/i18n/vi/*.dart` | One `const Map<String, String>` per feature area, merged in `l10n.dart` |

`MaterialApp` (both the main window and the tray mini-chat) gets
`locale: Locale(localeCode)` plus `L10nDelegate` and the `flutter_localizations`
`Global*` delegates, so Flutter's own widgets (date pickers, text-selection
menus, tooltips) localize too.

## Adding a string

```dart
Text(context.tr('Add channel'))                              // plain
Text(context.trArgs('Version {v}', {'v': svc.version}))      // interpolation
Text(context.trPlural(n, '{n} channel', '{n} channels'))     // plural
```

Then add the Vietnamese to the `vi/` file for that area. Outside widgets — tray
labels, OS notifications, provider-side messages — use `L10n.global.t(...)`,
which `localeCodeProvider` keeps in sync.

Wrapping makes an expression non-const, so drop `const` from the widget and any
enclosing const list. That is the single most common compile error when
localizing an existing screen.

## What must never be wrapped

Logical keys and ids (`'general'`, `'telegram'`, route paths, prefs keys, JSON
field names), URLs and file paths, CLI commands, model ids, MCP tool names,
**LLM prompt text sent to agents**, debug/log strings, and brand names
(SenClaw, Telegram, Feishu, …). Data that flows through the app — chat message
content, note bodies, board and card titles, wiki pages, agent-defined form
fields — is user content, not UI, and stays untouched.

## Verifying

```bash
cd desktop_app && flutter test test/language_setting_test.dart
```

[`test/language_setting_test.dart`](../desktop_app/test/language_setting_test.dart)
covers the default (English), the switch to Vietnamese and back, fallback for
untranslated keys, persistence across restarts, and `L10n.global` sync.

Every other widget test asserts English strings and runs with the default
language, so a correct localization change is a no-op for them — if one of
those tests goes red, a string was translated that should not have been, or a
`const` was left behind.

The repo-local script `scratchpad/check_i18n.py` (regenerate as needed) greps
every wrapped key out of `lib/` and reports the ones with no Vietnamese entry.
