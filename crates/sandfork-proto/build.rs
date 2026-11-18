fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A vendored protoc, so building needs no system package.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    let proto = "../../proto/sandfork.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
