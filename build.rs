fn main() {
    println!("cargo:rerun-if-changed=assets");
    embed_resource::compile("assets/app.rc", embed_resource::NONE);
}
