fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .codec_path("tonic_prost::ProstCodec")
        .compile_protos(&["proto/mcp_gateway.proto"], &["proto"])?;
    Ok(())
}
