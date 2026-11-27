fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=windows.rc");
    println!("cargo:rerun-if-changed=../../packaging/windows/dotdir.ico");

    embed_resource::compile_for("windows.rc", ["dotdir"], embed_resource::NONE)
        .manifest_required()
        .expect("compile the DotDir Windows icon resource");
}
