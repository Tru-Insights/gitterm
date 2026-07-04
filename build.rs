fn main() {
    compile_agent_protos();

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "GitTerm");
        res.set(
            "FileDescription",
            "Git status viewer with integrated terminal",
        );
        res.set("CompanyName", "GitTerm");
        res.compile().unwrap();
    }

    #[cfg(target_os = "macos")]
    {
        // Embed Info.plist so macOS grants microphone permission to the binary
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let plist_path = std::path::Path::new(&manifest_dir).join("Info.plist");
        if plist_path.exists() {
            println!(
                "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
                plist_path.display()
            );
        }
    }
}

fn compile_agent_protos() {
    let proto_file = "proto/gitterm/agent/v1/agent.proto";
    println!("cargo:rerun-if-changed={proto_file}");
    println!("cargo:rerun-if-changed=proto/gitterm/agent/v1");

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto_file], &["proto"])
        .expect("compile gitterm-agent protobufs");
}
