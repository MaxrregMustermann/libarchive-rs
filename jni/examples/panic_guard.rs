//! Whether the `catch_unwind` guard at the JNI boundary is live in this build.
//!
//!     cargo run --profile release        --example panic_guard   # SIGABRT, no output
//!     cargo run --profile release-unwind --example panic_guard   # caught, exit 0
//!
//! **This is an example rather than a test, and it has to be.** Cargo forces
//! `-C panic=unwind` on test, bench and build-script targets whatever the profile says, so
//! `cargo test` reports "caught" under both profiles and a test written this way passes no
//! matter which profile the app links. That is not a subtlety: it is why a test asserting
//! this was written, passed, and was still wrong. An example is built by the ordinary
//! profile, so the profile under test is the profile in force.
//!
//! What it checks is the mechanism, not the library: a `catch_unwind` guard of the same shape
//! as `jni::guarded`, around a panic of the kind the crate's deny-lints cannot catch — an index
//! past the end of a buffer, which is what a length field from a corrupt archive produces.
//!
//! The exit status is the verdict: 0 means the guard ran, 134 means the process was killed by
//! SIGABRT and nothing ran.

/// The same shape as `jni::guarded`, without the JNI types.
fn guarded<T>(body: impl FnOnce() -> T) -> Result<T, &'static str> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => Ok(value),
        Err(_) => Err("panic was caught by the guard"),
    }
}

/// A panic the crate's lints cannot prevent: an index derived from a corrupt length field.
fn index_past_the_end() -> u8 {
    let bytes: Vec<u8> = Vec::new();
    bytes[7]
}

fn main() {
    match guarded(index_past_the_end) {
        Ok(value) => {
            eprintln!("UNEXPECTED: the guard returned Ok({value}) from a panicking body");
            std::process::exit(1);
        }
        Err(caught) => {
            println!("{caught}");
            println!("reached the end of main: this build unwinds, so `guarded` is live");
            println!(
                "Now check this was the profile app/build.gradle builds the cdylib with; if it \
                 was not, the guard is dead code in the binary that ships."
            );
        }
    }
}