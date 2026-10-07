fn main() {
    // The rust-i18n macro reads locales/*.yml at expansion time but declares no
    // dependency — yml changes would not trigger recompilation. Add
    // directory-level tracking (cargo detects directories recursively) so
    // changing a yml rebuilds.
    println!("cargo:rerun-if-changed=locales");
}
