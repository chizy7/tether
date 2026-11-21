//! Generates the gRPC service from `proto/tether.proto`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The vendored compiler, so `cargo build` never needs protoc installed.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    let proto = "../../proto/tether.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_prost_build::configure().compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
