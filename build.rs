fn main() {
    let target = std::env::var("TARGET").expect("Cargo supplies the compilation target");
    println!("cargo:rustc-env=POS3QL_BUILD_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
