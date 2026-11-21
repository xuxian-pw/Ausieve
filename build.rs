fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/warn.ico");
    embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
}
