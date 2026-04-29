fn main() {
    let src_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap(),
    )
    .join("flecs_src");

    cc::Build::new()
        .file(src_dir.join("flecs.c"))
        .file(src_dir.join("flecs_helper.c"))
        .include(&src_dir)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-w")
        .opt_level(3)
        .compile("flecs");

    println!("cargo:rerun-if-changed={}", src_dir.join("flecs.c").display());
    println!("cargo:rerun-if-changed={}", src_dir.join("flecs.h").display());
    println!("cargo:rerun-if-changed={}", src_dir.join("flecs_helper.c").display());
    println!("cargo:rerun-if-changed={}", src_dir.join("flecs_helper.h").display());
}
