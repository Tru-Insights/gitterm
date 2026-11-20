fn main() {
    #[cfg(feature = "runtime")]
    {
        let proto_file = "../../proto/gitterm/agent/v6/agent.proto";
        println!("cargo:rerun-if-changed={proto_file}");
        println!("cargo:rerun-if-changed=../../proto/gitterm/agent/v6");
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
        std::env::set_var("PROTOC", protoc);
        tonic_build::configure()
            .build_client(true)
            .build_server(true)
            .compile_protos(&[proto_file], &["../../proto"])
            .expect("compile gitterm-agent protobufs");
    }
}
