fn main() {
    println!("cargo:rerun-if-changed=assets/app_icon.res");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let res_path = std::path::Path::new(&manifest_dir)
            .join("assets")
            .join("app_icon.res");
        if res_path.exists() {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
    }
}
