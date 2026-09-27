# SenClaw daemon — build/run/test targets only. The desktop app (Flutter),
# the web UI and the sen-* runtimes each live in their own sibling repository
# now (see docs/runtime-protocol.md and the migration plan); their targets
# moved with them.

.PHONY: run run-release test test-sdks clean-target-cache

run:
	cargo run

# Release build. Not needed for local model inference anymore — that runs in
# a sen-* runtime, a separate process — but still the right build for a
# realistic perf check of the daemon itself.
run-release:
	cargo run --release

test:
	cargo test --workspace

# The two SDKs shipped from this repo (`app-space-sdk`, `crates/sen-runtime-sdk`)
# are each their own Cargo workspace root (excluded from the top-level one so
# that editing this repo's manifest can never break a `cargo build` in a sen-*
# repo depending on them by path) — `cargo test --workspace` does not reach
# them, so test both explicitly.
test-sdks:
	cargo test --manifest-path app-space-sdk/Cargo.toml
	cargo test --manifest-path crates/sen-runtime-sdk/Cargo.toml

clean-target-cache:
	@echo "[clean] removing target/debug and incremental caches"
	@rm -rf target/debug target/release/incremental target/release/build/*-*/incremental 2>/dev/null || true
	@du -sh target 2>/dev/null || true
