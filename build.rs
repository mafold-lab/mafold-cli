// The `mafold` binary's main thread gets the same 8 MB of stack on every OS.
//
// macOS and Linux give a main thread 8 MB; Windows gives it 1 MB, fixed at
// link time. `main` is one large `#[tokio::main] async fn` polled on that
// thread, and a debug build of it overflows 1 MB before it does anything: on
// Windows `mafold bash-hook` (and every other subcommand) died at startup with
// «thread 'main' has overflowed its stack», which claude treats as a hook that
// had nothing to say. A release build fits today only because optimisation
// shrinks the frames — a margin nobody measures. So: ask the linker for 8 MB.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("windows-msvc") {
        println!("cargo:rustc-link-arg-bin=mafold=/STACK:8388608");
    } else if target.ends_with("windows-gnu") || target.ends_with("windows-gnullvm") {
        println!("cargo:rustc-link-arg-bin=mafold=-Wl,--stack,8388608");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
