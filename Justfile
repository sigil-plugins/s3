set shell := ["bash", "-euo", "pipefail", "-c"]

wasm_tools := env_var_or_default("WASM_TOOLS", "wasm-tools")
sigil := env_var_or_default("SIGIL", "sigil")
python := env_var_or_default("PYTHON", "python3")

build:
    root="$(pwd -P)"; cargo_home="${CARGO_HOME:-$HOME/.cargo}"; rustflags="${RUSTFLAGS:-} --remap-path-prefix=${root}=/workspace --remap-path-prefix=${cargo_home}=/cargo"; RUSTFLAGS="${rustflags# }" cargo build --release --target wasm32-unknown-unknown --locked
    {{wasm_tools}} component new target/wasm32-unknown-unknown/release/sigil_plugin_s3.wasm -o plugin.wasm
    {{wasm_tools}} validate --features all plugin.wasm
    {{wasm_tools}} component targets wit --world sigil:s3/s3@0.1.0 plugin.wasm

sdk-drift:
    ./scripts/check-sdk-lock.sh

check: sdk-drift
    cargo fmt --all -- --check
    cargo test --locked
    cargo clippy --all-targets --locked -- -D warnings
    just build

sigil-check: check
    {{sigil}} plugin validate plugin.toml
    {{sigil}} plugin inspect plugin.toml --format json

dist: check
    mkdir -p dist
    {{python}} scripts/pack.py plugin.toml dist

release-dist source_commit: check
    mkdir -p dist
    {{python}} scripts/pack.py plugin.toml dist --source-commit "{{source_commit}}"

reproducible:
    first="$(sha256sum plugin.wasm)"; just build; test "$first" = "$(sha256sum plugin.wasm)"
