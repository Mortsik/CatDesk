//! Freezes compile-time build identity into the crate.
//!
//! Always emits every variable with a literal `"unknown"` skeleton here; real
//! git probing with safe fallbacks lands on top of this in a follow-up commit.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=CATDESK_GIT_SHA=unknown");
    println!("cargo:rustc-env=CATDESK_GIT_BRANCH=unknown");
    println!("cargo:rustc-env=CATDESK_BUILD_TIMESTAMP=unknown");
}
