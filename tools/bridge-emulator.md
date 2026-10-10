# Running the bridge on the API 24 emulator

Everything needed to take `librust_archive.so` from this repository into an APK, onto a running
API 24 emulator, and to read an entry back out through JNI with the bytes in the log.

`tools/bridge-proof.sh` does the whole sequence. This file is what it is doing and why each step is
the way it is, because three of them look wrong and are not.

---

## 1. There are two Android SDKs, and Gradle uses the one that has no emulator

| Directory | What is in it | Who wants it |
|---|---|---|
| `/opt/android-sdk` | `ndk/30.0.16248370`, `build-tools/35.0.0`…`37.0.0`, `platforms/android-34`…`37.2`, `cmake/` | Gradle. `local.properties` has `sdk.dir=/opt/android-sdk` |
| `/home/user/Android/Sdk` | `emulator/`, `platform-tools/`, `system-images/`, `skins/`, `sources/` | `adb` and the emulator |

`/opt/android-sdk` has **no NDK at the top level and no emulator**; `/home/user/Android/Sdk` has
**no NDK at all** and no `platforms/`. The split is deliberate on this machine. Do not try to make
them one directory.

So the two things that must not be confused:

    # Building. NDK from /opt, driven by Gradle.
    export JAVA_HOME=/opt/jdk/jdk-21.0.12.1+1
    export ANDROID_NDK_HOME=/opt/android-sdk/ndk/30.0.16248370

    # Running. adb and the emulator from /home/user/Android/Sdk.
    export PATH=/home/user/Android/Sdk/platform-tools:/home/user/Android/Sdk/emulator:$PATH

`cargo-ndk` only needs the NDK, so `cargo ndk -t x86 -t arm64-v8a ... build --release` works with
`ANDROID_NDK_HOME` pointing at `/opt` alone.

## 2. The AVD is `Android_7`, and it is x86

    /home/user/Android/Sdk/emulator/emulator -avd Android_7 -gpu angle_indirect -no-window \
        -accel on

`-gpu angle_indirect` and `-no-window` are what make it startable with no X server;
`/dev/kvm` has to exist or `-accel on` silently falls back to software emulation, which boots in
minutes instead of seconds and is not the same machine.

It must be launched under `setsid` so that it survives the shell timeout that killed it twice before:

    setsid /home/user/Android/Sdk/emulator/emulator -avd Android_7 -gpu angle_indirect \
        -no-window -accel on > /home/user/emulator.log 2>&1 &

Do not reboot it to free memory. It is 1.3 GB and it is the only place the bridge can be proved.
If it does go away, mail the orchestrator before touching it: they use it for JNI checks.

**The emulator is x86.** `ro.product.cpu.abi` is `x86` and `ro.build.version.sdk` is 24. So:

- `x86` is built **and run** here.
- `arm64-v8a`, `armeabi-v7a` and `x86_64` are built here and **cannot be run** here.

Say which of the two you mean in any report. "All four ABIs work" is not a claim anyone can check on
this machine, and the one that can be checked is the one that matters.

## 3. The app gets the library from this repository, not from its own copy

`app/build.gradle` has a `buildRustArchive` task that runs `cargo ndk` against a checkout of
libarchive-rs. Where that checkout is comes from a property, so a branch can be proved before it is
merged:

    ./gradlew --no-daemon -PlibarchiveRsPath=/path/to/libarchive-rs :app:assembleDebug

The default is the checkout beside the app, `/home/user/libarchive-rs`, which is the integration
branch. The task puts its output in `app/build/rustArchive/jniLibs/<abi>/` and packages it as an
ordinary native library, so no step copies anything into `src/main/jniLibs` by hand.

`assembleDebug` alone does **not** run the task unless the build wires it in; run
`:app:buildRustArchive` first when you want the library rebuilt and nothing else.

## 4. Running the proof

    # 1. The APK.
    ./gradlew --no-daemon -PlibarchiveRsPath=$PWD :app:assembleDebug
    adb install -r app/build/outputs/apk/debug/app-debug.apk

    # 2. The instrumentation. `androidx.test` is not in the offline Gradle cache, so this is a bare
    #    platform Instrumentation rather than AndroidJUnitRunner.
    adb logcat -c
    adb shell am instrument -w \
        me.zhanghai.android.files/me.zhanghai.android.files.bridge.BridgeInstrumentation

    # 3. What it said.
    adb logcat -d -s BridgeProof

The component lives in `app/src/debug/`, so it is in the debug APK and in no release build.

What it asserts, and where each assertion comes from:

| Log line | What it proves |
|---|---|
| `librust_archive.so loaded` | the `.so` is in the APK and its symbols resolved |
| `BYTES <name> = <n> bytes, hex[0..16)=…, text=…` | the bytes themselves arrived, not just a length. This is the acceptance criterion for a JNI read |
| `read 64 entries in order through ArchiveFileSystem: …, 1 archive opens` | the archive is held open across per-file reads |
| `read the same 64 entries in reverse: …, N archive opens` | the first number is not an accident: an archive only moves forward, so reading backwards has to reopen |

The reverse pass is the control. Without it, "1 open" could mean the test never opened anything.

## 5. What this cannot prove

- The other three ABIs. They are built and packaged; they are not executed.
- Anything about `panic = "abort"`. In a release profile a Rust panic aborts the process instead of
  unwinding into the `IOException` the bridge throws, so the panic-to-exception path is only
  exercised by a debug build. That is worth knowing before reading a clean debug run as proof that
  the release build is safe.
- Release-mode behaviour of the Kotlin inlining or of the linker, which the debug APK does not use.

## 6. When the emulator will not boot

The two failures worth distinguishing, because they look the same in a log:

    # No /dev/kvm: it starts and then sits at "info: guest not yet initialized" for minutes.
    ls -l /dev/kvm

    # The AVD is locked by another emulator process, or a previous one is still running.
    /home/user/Android/Sdk/platform-tools/adb devices      # more than one emulator-555N
    /home/user/Android/Sdk/emulator/emulator -list-avds

Report the error rather than a partial result. A bridge that was not proved is worth knowing about;
a bridge that was proved against an emulator that was really the wrong one is not.