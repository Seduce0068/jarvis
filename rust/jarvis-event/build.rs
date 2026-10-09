fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let proto_dir = std::path::Path::new(&out_dir).join("proto");
    std::fs::create_dir_all(&proto_dir)?;

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&proto_dir)
        .compile_protos(&["proto/event.proto"], &["proto"])?;
    Ok(())
}
