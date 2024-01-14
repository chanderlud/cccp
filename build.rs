use std::io::Result;

fn main() -> Result<()> {
    #[cfg(unix)]
    std::env::set_var("PROTOC", "/bin/protoc");
    prost_build::compile_protos(&["src/items.proto"], &["src/"])?;

    Ok(())
}
