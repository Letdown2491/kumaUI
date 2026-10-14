// The build stamp, re-evaluated whenever it changes: build.sh passes
// KUMA_GIT_SHA into the container for every cargo invocation, and a
// plain option_env! in a library bakes one commit's stamp and goes
// stale while the apps (which recompile far more often) keep reporting
// it. rustc-env re-bakes the value into this crate's compile whenever
// the incoming variable changes, so env! below always reads the build
// that is actually running (and "unknown" when cargo runs without the
// stamp, the same fallback option_env! gave the apps).
fn main() {
    println!("cargo:rerun-if-env-changed=KUMA_GIT_SHA");
    println!(
        "cargo:rustc-env=KUMA_GIT_SHA={}",
        std::env::var("KUMA_GIT_SHA").unwrap_or_else(|_| "unknown".to_string()),
    );
}
