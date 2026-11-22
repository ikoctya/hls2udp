#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$HOME/tools/zig:$PATH"
cargo zigbuild --release --target x86_64-pc-windows-gnu
ls -lh target/x86_64-pc-windows-gnu/release/Hls2Udp.exe
