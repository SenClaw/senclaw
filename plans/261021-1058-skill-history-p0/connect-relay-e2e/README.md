# SenClaw Connect over a local relay — skill history end to end (2026-10-21)

Not part of any test suite: it opens a hub, a daemon and a relay, which no repo test may do. Run by hand.

1. Hub: `senclaw-hub-connector` → `go build -o hub ./cmd/server`; `SHARED_PORT=:38080 ./hub` (it writes `hub.db` in its cwd).
2. `curl -X POST :38080/v1/channels/register` → `channel_id`, `access_token`; add a base64 32-byte `encryptionKey` and write `creds.json` = `{hubUrl, channelId, accessToken, encryptionKey}`.
3. Scratch daemon (`start-scratch-daemon.sh`: own HOME / SENCLAW_HOME / cwd / ports 38788-38789, seatbelt guard against 18788-18790). Start it once so the DB exists, stop it, insert a row into `channels` (`platform_type='app'`, `credentials_json` = creds.json), start it again: the log says `AppChannel … Connected to relay`.
4. Seed a managed skill with a few revisions (`POST /api/skills/create`, overwrite, `PUT …/readme` with an override phrase).
5. Copy `relay_skill_history_e2e_test.dart` into `senclaw-connect/test/` and run `E2E_CREDS=<creds.json> flutter test test/relay_skill_history_e2e_test.dart`. It mocks `flutter_secure_storage`, lifts `HttpOverrides` (flutter_test blocks sockets), pairs through `ConfigService`, and drives `ApiClient` / `PluginsApi` through the real encrypted relay. Delete the file afterwards.

Result 2026-10-21: 12 of 12 checks passed against the release daemon built from `feat/skill-history`.
