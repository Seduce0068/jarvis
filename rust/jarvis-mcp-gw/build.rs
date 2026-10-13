fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let proto_dir = std::path::Path::new(&out_dir).join("proto");
    std::fs::create_dir_all(&proto_dir)?;

    tonic_prost_build::configure()
        .codec_path("tonic_prost::ProstCodec")
        .out_dir(&proto_dir)
        .compile_protos(&["proto/mcp_gateway.proto"], &["proto"])?;
    Ok(())
}
