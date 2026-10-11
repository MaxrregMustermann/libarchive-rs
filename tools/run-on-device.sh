#!/bin/sh
# Build the workload benchmark for a real device, push it, run it, and bring the numbers back.
#
#     tools/run-on-device.sh [corpus-directory]
#
# The point of this script is that the host and the device run **the same source at the same
# revision**, so the two sets of numbers are comparable. A benchmark that is rebuilt differently
# for each machine is not measuring the machine, it is measuring the build.
#
# ## What an Android run can and cannot tell you
#
# The AVD this repository is proved against is `Android_7`: **x86**, API 24, API 24's
# Bionic libc, 32-bit pointers, running on the host CPU through KVM. That makes it a good stand-in
# for two things and a bad stand-in for a third:
#
#   * **JNI boundary cost.** Real, and worth measuring: every write the ZIP writer makes crosses
#     into Java, and `benches/zip.rs` asserts the sink-write count precisely because each crossing
#     costs about a microsecond. The host benchmark counts writes; only a device charges for them.
#   * **32-bit behaviour.** `arm64-v8a` is what the app ships, but it cannot be run here. The x86
#     ABI is 32-bit, so at least the pointer width, the allocator and the atomics are being
#     exercised rather than assumed.
#   * **Codec throughput on a real phone.** It cannot. This is x86 on the same silicon as the
#     workstation. A number from here says nothing about how fast an arm64 phone is, and should
#     never be quoted as if it did.
#
# Say which of the three a device number is when you report it.
#
# ## Requirements
#
#   * `ANDROID_NDK_HOME=/opt/android-sdk/ndk/30.0.16248370` (cargo-ndk only needs the NDK)
#   * `cargo ndk` 4.x on PATH
#   * `adb` from `/home/user/Android/Sdk/platform-tools` on PATH, with a device attached
#   * the `i686-linux-android` Rust target, which is what the x86 AVD runs
#
# Everything is written under `/home/user`; nothing here writes to `/tmp`.

set -eu

CORPUS="${1:-/home/user/bench-corpus}"
ADB="${ADB:-/home/user/Android/Sdk/platform-tools/adb}"
REMOTE="${REMOTE:-/data/local/tmp}"
REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Deliberately not /home/user/cargo-target/<agent>: this is the orchestrator's build, and the
# per-agent directories are hard-link seeds of it.
TARGET_DIR="${CARGO_TARGET_DIR:-/home/user/cargo-target-device}"
# `x86` because that is the ABI the API 24 AVD reports. `arm64-v8a` builds but cannot be run here.
ABI="${ABI:-x86}"
RUST_TARGET="${RUST_TARGET:-i686-linux-android}"

export PATH="$HOME/.cargo/bin:$PATH"
export TMPDIR=/home/user/tmp

say() { printf '\n== %s\n' "$1"; }

say "environment"
command -v cargo-ndk >/dev/null || { echo "cargo-ndk is not installed" >&2; exit 1; }
[ -n "${ANDROID_NDK_HOME:-}" ] || { echo "set ANDROID_NDK_HOME to the NDK root" >&2; exit 1; }
"$ADB" get-state >/dev/null || { echo "no device attached" >&2; exit 1; }
DEVICE_ABI=$("$ADB" shell getprop ro.product.cpu.abi | tr -d '\r')
DEVICE_SDK=$("$ADB" shell getprop ro.build.version.sdk | tr -d '\r')
printf '  ndk            %s\n' "$ANDROID_NDK_HOME"
printf '  device         abi=%s sdk=%s\n' "$DEVICE_ABI" "$DEVICE_SDK"
printf '  building for   %s (%s)\n' "$ABI" "$RUST_TARGET"
if [ "$DEVICE_ABI" != "$ABI" ]; then
    printf '  NOTE           the device is %s but this builds %s; the binary will not run\n' \
        "$DEVICE_ABI" "$ABI"
fi

say "corpus"
[ -f "$CORPUS/big.zip" ] || { echo "missing $CORPUS/big.zip - see tools/make_bigzip.py" >&2; exit 1; }
[ -f "$CORPUS/solid400big.7z" ] || { echo "missing $CORPUS/solid400big.7z - see tools/make-corpus.sh" >&2; exit 1; }
ls -l "$CORPUS/big.zip" "$CORPUS/solid400big.7z"

say "build"
mkdir -p "$TARGET_DIR"
# Baked in so the run can say what it was, even though there is no git on the device.
REVISION=$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)
export LIBARCHIVE_REVISION="$REVISION"
printf '  revision       %s\n' "$REVISION"
cargo ndk -t "$ABI" build --release --offline \
    --example bench_workloads --target "$RUST_TARGET" 2>&1 | tail -5
BINARY="$TARGET_DIR/$RUST_TARGET/release/examples/bench_workloads"
[ -x "$BINARY" ] || { echo "no binary at $BINARY" >&2; exit 1; }
ls -l "$BINARY"

say "push the corpus"
# The device has far less storage than the host; the benchmark reads them once per run and
# writes nothing, so pushing is enough.
"$ADB" shell "mkdir -p $REMOTE/bench" >/dev/null
"$ADB" push -q "$CORPUS/big.zip" "$REMOTE/bench/big.zip"
"$ADB" push -q "$CORPUS/solid400big.7z" "$REMOTE/bench/solid400big.7z"
"$ADB" push -q "$BINARY" "$REMOTE/bench_workloads"
"$ADB" shell "chmod 755 $REMOTE/bench_workloads"

say "run on device"
# `LIBARCHIVE_REVISION` is passed through the environment rather than baked in twice over, so
# the number on the device and the number on the host name the same commit.
"$ADB" shell "cd $REMOTE && LIBARCHIVE_REVISION=$REVISION ./bench_workloads \
    $REMOTE/bench/big.zip $REMOTE/bench/solid400big.7z" | tr -d '\r'

say "the same binary on the host, for comparison"
# Run the host build last so a device number is never taken without its host counterpart to
# sit next to it in the same report.
LIBARCHIVE_REVISION="$REVISION" "$BINARY_FOR_HOST" 2>/dev/null || \
    LIBARCHIVE_REVISION="$REVISION" \
    /home/user/cargo-target-libarchive/release/examples/bench_workloads \
    "$CORPUS/big.zip" "$CORPUS/solid400big.7z" 2>/dev/null || \
    echo "  (no host build; run: cargo run --release --offline --example bench_workloads -- $CORPUS/big.zip $CORPUS/solid400big.7z)"

printf '\nRead the two blocks above together. Where they disagree by more than the build\n'
printf 'explains, that difference is the thing worth investigating.\n'