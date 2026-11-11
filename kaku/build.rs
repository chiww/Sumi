fn main() {
    // rust_i18n::i18n! embeds ../locales at compile time but does not tell
    // cargo, so a translation edit would otherwise ship the stale strings.
    println!("cargo:rerun-if-changed=../locales");
    println!("cargo:rerun-if-changed=build.rs");
}
