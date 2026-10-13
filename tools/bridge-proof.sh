#!/usr/bin/env bash
#
# Proves the JNI bridge on the API 24 emulator: builds librust_archive.so for every ABI the app
# ships, assembles the app with that library in it, installs it, and runs an instrumentation that
# reads entries back out through the bridge with the bytes in logcat.
#
# This is a host-side script because a host-side `cargo test` cannot do this job. Everything that
# goes wrong at a JNI boundary is invisible from the host: a symbol that does not match the Kotlin
# declaration, a signature JNI resolves by string, a local-reference table that overflows, and above
# all a panic, which on Android aborts the process instead of raising a Java exception.
#
# Read tools/bridge-emulator.md first. The two things that bite are the split Android SDK (Gradle
# builds out of /opt/android-sdk, adb and the emulator live in /home/user/Android/Sdk) and the fact
# that this emulator is x86, so the other three ABIs are proved by building them and not by running
# them.
#
#   tools/bridge-proof.sh                       # build, install, run, report
#   tools/bridge-proof.sh --skip-build          # reuse the APK that is already built
#   tools/bridge-proof.sh --abi arm64-v8a       # install for another ABI (see the caveat below)
#   tools/bridge-proof.sh --no-emulator         # only build and report what was built
#
# Exits 0 when the instrumentation passes, 1 when it fails, 2 when the emulator is not there.

set -o errexit
set -o nounset
set -o pipefail

readonly ABIS=(arm64-v8a armeabi-v7a x86_64 x86)
readonly EMULATOR_ABI=x86
readonly REPORT_TAG=BridgeProof
readonly INSTRUMENTATION=me.zhanghai.android.files/me.zhanghai.android.files.bridge.BridgeInstrumentation

# The checkout of libarchive-rs to prove. It defaults to the integration checkout beside the app,
# and is overridable so that a branch can be proved before it is merged:
#
#     LIBARCHIVE_RS_PATH=/home/user/wt/pip tools/bridge-proof.sh
#
readonly LIBARCHIVE_RS_PATH=${LIBARCHIVE_RS_PATH:-/home/user/libarchive-rs}
export LIBARCHIVE_RS_PATH
# This script's own directory, so `tools/make-bridge-fixtures.sh` is found next to it rather than
# relative to whatever the caller happened to be in.
readonly REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Where the fixtures are built on the host before being pushed. Under the user's home rather than
# /tmp, which is a 7.7 GiB tmpfs that filled to 98% during wave 2.
readonly FIXTURES_HOST_DIR=${FIXTURES_HOST_DIR:-/home/user/bridge-fixtures}

# The split SDK. See tools/bridge-emulator.md section 1.
readonly BUILD_SDK=/opt/android-sdk
readonly RUN_SDK=/home/user/Android/Sdk
readonly NDK=$BUILD_SDK/ndk/30.0.16248370
readonly APP=${APP:-/home/user/TheFileManager}

skip_build=0
want_emulator=1
run_abi=$EMULATOR_ABI

for argument in "$@"; do
    case $argument in
        --skip-build) skip_build=1 ;;
        --no-emulator) want_emulator=0 ;;
        --abi=*) run_abi=${argument#--abi=} ;;
        *) echo "unknown argument: $argument" >&2; exit 2 ;;
    esac
done

export JAVA_HOME=${JAVA_HOME:-/opt/jdk/jdk-21.0.12.1+1}
export ANDROID_NDK_HOME=$NDK
export PATH=$RUN_SDK/platform-tools:$RUN_SDK/emulator:$HOME/.cargo/bin:$PATH

say() {
    printf '\n=== %s\n' "$*"
}

say "environment"
echo "cargo        $(cargo --version)"
echo "java         $($JAVA_HOME/bin/java -version 2>&1 | head -1)"
echo "ndk          $NDK"
echo "libarchive-rs $LIBARCHIVE_RS_PATH"
echo "app          $APP"

if (( skip_build == 0 )); then
    # All four ABIs, through cargo-ndk. This is the "the cdylib builds for the ABIs the app ships"
    # half of the claim, and it is checked rather than assumed: cargo-ndk exits 0 having built
    # nothing if a target is unknown to the toolchain.
    say "building the cdylib for ${ABIS[*]}"
    targets=()
    for abi in "${ABIS[@]}"; do
        targets+=(-t "$abi")
    done
    cargo ndk "${targets[@]}" -o "$APP/app/build/rustArchive/jniLibs" build --release \
        --package libarchive-jni
    for abi in "${ABIS[@]}"; do
        so=$APP/app/build/rustArchive/jniLibs/$abi/librust_archive.so
        if [[ ! -f $so ]]; then
            echo "MISSING: $abi produced no librust_archive.so" >&2
            exit 1
        fi
        printf '  %-12s %10d bytes\n' "$abi" "$(stat -c %s "$so")"
    done
fi

if (( want_emulator == 0 )); then
    say "not running on the emulator, as asked"
    exit 0
fi

say "emulator"
if ! adb devices | grep -q '^emulator-[0-9]*[[:space:]]*device'; then
    echo "no emulator is attached. tools/bridge-emulator.md section 2 says how to start one." >&2
    exit 2
fi
echo "devices      $(adb devices | tr '\n' ' ')"
echo "sdk          $(adb shell getprop ro.build.version.sdk | tr -d '\r')"
echo "abi          $(adb shell getprop ro.product.cpu.abi | tr -d '\r')"
if [[ $(adb shell getprop ro.product.cpu.abi | tr -d '\r') != "$run_abi" ]]; then
    echo "this emulator is not $run_abi, so it cannot run an APK installed for $run_abi" >&2
    exit 2
fi

say "assembling"
# --no-daemon because an idle Gradle daemon costs 1.6 GB on a machine running seven agents, and
# that is the difference between this finishing and being OOM-killed.
(cd "$APP" && ./gradlew --no-daemon --offline -PlibarchiveRsPath="$LIBARCHIVE_RS_PATH" :app:assembleDebug)
apk=$APP/app/build/outputs/apk/debug/app-debug.apk
[[ -f $apk ]] || { echo "no APK at $apk" >&2; exit 1; }
echo "apk          $(stat -c %s "$apk") bytes"

say "installing"
adb install -r "$apk"

say "making the fixtures"
# Built here rather than checked in, so the bytes on the device were made by tar, gzip, bzip2, xz
# and 7z and not by the code under test. See tools/make-bridge-fixtures.sh for why that matters and
# for where the ground truth comes from.
"$REPO_DIR/tools/make-bridge-fixtures.sh" "${FIXTURES_HOST_DIR:?}"

say "pushing the fixtures"
# BridgeInstrumentation.kt requires every one of these to be present and treats a missing one as a
# failure rather than a skip, because a silent skip looks exactly like a pass. So the set of files
# pushed here and the set the instrumentation names have to agree, and the fixtures are named in
# both.
fixtures_dir=/storage/emulated/0/Android/data/me.zhanghai.android.files/files
adb shell "mkdir -p $fixtures_dir"
for fixture in \
    "$LIBARCHIVE_RS_PATH/libarchive/tests/data/legacy/arc/store.arc" \
    "$LIBARCHIVE_RS_PATH/libarchive/tests/data/legacy/ice/license_lha.ice" \
    "$FIXTURES_HOST_DIR/license.txt" \
    "$FIXTURES_HOST_DIR/plain.tar" \
    "$FIXTURES_HOST_DIR/plain.tar.gz" \
    "$FIXTURES_HOST_DIR/plain.tar.xz" \
    "$FIXTURES_HOST_DIR/plain.tar.bz2" \
    "$FIXTURES_HOST_DIR/plain.7z"
do
    [[ -f $fixture ]] || { echo "MISSING: $fixture" >&2; exit 1; }
    adb push -q "$fixture" "$fixtures_dir/" >/dev/null
done
# A ZIP made by the host's own zip(1). The instrumentation round-trips a ZIP through this library's
# writer and reader, which proves the two agree with each other; this one is a ZIP made by something
# else, which is the only kind of agreement worth anything.
if command -v zip >/dev/null && command -v unzip >/dev/null; then
    work=$(mktemp -d "${TMPDIR:-/home/user/tmp}/bridge-foreign-XXXXXX")
    cp "$FIXTURES_HOST_DIR/license.txt" "$work/"
    (cd "$work" && zip -q "$FIXTURES_HOST_DIR/foreign.zip" license.txt)
    adb push -q "$FIXTURES_HOST_DIR/foreign.zip" "$fixtures_dir/" >/dev/null
    rm -rf "$work"
    echo "  foreign.zip $(unzip -l "$FIXTURES_HOST_DIR/foreign.zip" | tail -1)"
else
    echo "  zip/unzip not on PATH, so no foreign.zip; the instrumentation skips that one"
fi
echo "  pushed to $fixtures_dir"

say "running the instrumentation"
adb logcat -c
adb shell am instrument -w "$INSTRUMENTATION" || true

say "logcat -d -s $REPORT_TAG"
# -v brief keeps one line per report, and the payload is the point of the exercise: this is where
# the bytes coming back across JNI are supposed to be visible.
adb logcat -d -v brief -s "$REPORT_TAG" || true

say "verdict"
# `I/BridgeProof( 1234): PASS` in brief format: the tag is everything up to the first colon and the
# payload is everything after it. There is exactly ONE colon in that line, so `cut -d: -f3-` - which
# is what this script used to do - is the empty string, and the script exited 1 on every run
# whether the instrumentation had passed or not. That is the whole of a false negative: it looks
# like a broken bridge and it is a broken one-liner. The instrumentation logs exactly `PASS` on
# success and `FAIL: ...` otherwise, and logcat is filtered to its tag, so this is its verdict
# rather than a framework message.
verdict=$(adb logcat -d -v brief -s "$REPORT_TAG" | grep -v '^---' | tail -1 | sed 's/^[^:]*: //')
echo "$verdict"
if [[ $verdict != PASS ]]; then
    exit 1
fi