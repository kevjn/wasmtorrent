set -ex
curl https://sh.rustup.rs -sSf | sh -s -- -y && . "$HOME/.cargo/env"
wasm-pack build

