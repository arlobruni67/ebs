#!/bin/bash
# Regenerates crates/engine/src/ff/gen.rs from the FFmpeg headers (needs zig).
set -e
cd "$(dirname "$0")"
export ZIG_LOCAL_CACHE_DIR='D:\EBS-rs\.tmp\zig' ZIG_GLOBAL_CACHE_DIR='D:\EBS-rs\.tmp\zig'
/d/tools/zig-x86_64-windows-0.17.0/zig.exe cc -target x86_64-windows-gnu -I /d/ArloCapture/third_party/ffmpeg/include ffgen.c -o ../.tmp/ffgen.exe
../.tmp/ffgen.exe > ../crates/engine/src/ff/gen.rs
echo "wrote gen.rs ($(wc -l < ../crates/engine/src/ff/gen.rs) lines)"
