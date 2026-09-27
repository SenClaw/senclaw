# Channel App Implementation Tasks

## Phase 1: Backend Infrastructure (Rust)
- [x] Add gRPC and Encryption dependencies to `Cargo.toml`
- [x] Set up `build.rs` for Protobuf compilation
- [x] Implement E2EE utility in `src/util/crypto.rs`
- [x] Implement gRPC Relay Client in `src/clawhub/relay_client.rs`
- [/] Integrate Relay Client with `MessageRouter` and `AgentPool`

## Phase 2: Pairing & Onboarding
- [x] Implement QR code payload generation logic
- [x] Add CLI command `senclaw channel connect` to display QR
- [ ] Add Web UI endpoint for QR display

## Phase 3: Flutter App Development
- [ ] Initialize Flutter project in `channel-app/`
- [ ] Set up gRPC client for mobile/desktop
- [ ] Implement QR scanning and secure key storage
- [ ] Implement E2EE decryption/encryption in Flutter
- [ ] Build Chat UI and History Sync

## Phase 4: Testing & Verification
- [ ] Unit tests for encryption (Rust & Flutter)
- [ ] Integration tests with a mock relay server
- [ ] Manual verification on Android/macOS
