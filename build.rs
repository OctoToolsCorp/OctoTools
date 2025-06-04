// use std::{env, path::PathBuf}; // Removed unused imports

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Comment out proto compilation as gRPC is no longer used --- 
    /*
    let proto_dir = "proto/jito-labs/mev-protos";
    // List all necessary proto files directly
    let proto_files = &[
        format!("{}/packet.proto", proto_dir),
        format!("{}/shared.proto", proto_dir),
        format!("{}/bundle.proto", proto_dir),
        format!("{}/searcher.proto", proto_dir),
        format!("{}/auth.proto", proto_dir),
        // Add others if they are imported by the above files
    ];

    println!("cargo:rerun-if-changed=build.rs");
    for proto_file in proto_files {
        println!("cargo:rerun-if-changed={}", proto_file);
    }

    tonic_build::configure()
        .build_client(true)
        // Ensure the source directory is included for imports within protos
        .compile(proto_files, &[proto_dir])?;
    */
    // --- End of commented out section ---

    println!("cargo:warning=Proto compilation in build.rs skipped (gRPC removed)."); // Add warning

    Ok(())
} 