#!/bin/sh
# Entry point for the xtask runner.
#
#   ./x dist            build everything and stage it for the SD card
#   ./x ci              fmt, gen-onvif --check, check-boards, check-literals, clippy, deny, test, build
#   ./x help            list every task
#
# This wrapper exists because the workspace pins `build.target` to the camera's
# ARMv5TE triple in .cargo/config.toml. xtask has to run on *this* machine, so
# it is built for the host explicitly and with the cross-compilation settings
# switched off.
set -eu

cd "$(dirname "$0")"

HOST=$(rustc -vV | sed -n 's/^host: //p')

exec cargo run \
    --quiet \
    --release \
    --manifest-path xtask/Cargo.toml \
    --target "$HOST" \
    --config "build.target=\"$HOST\"" \
    --config 'unstable.build-std=[]' \
    -- "$@"
