fn main() {
    println!("cargo:rerun-if-changed=src/account.proto");
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["src/account.proto"], &["src", "/usr/include"])
        .expect("failed to compile protos");
}
