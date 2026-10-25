//! Compile the vendored Firestore v1 protos (from github.com/googleapis/googleapis) with a
//! vendored `protoc`, so building needs no system tools.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: build scripts run single-threaded; protoc is located before any code generation.
    unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    let include = protoc_bin_vendored::include_path()?;
    tonic_prost_build::configure().build_server(true).build_client(true).compile_protos(
        &[std::path::PathBuf::from("proto/google/firestore/v1/firestore.proto")],
        &[std::path::PathBuf::from("proto"), include],
    )?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
