fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        embed_resource::compile("resources.rc", embed_resource::NONE);
    }
    println!("cargo:rerun-if-changed=resources.rc");
    println!("cargo:rerun-if-changed=../app.manifest");
}
