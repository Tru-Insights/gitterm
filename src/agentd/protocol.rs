// Generated tonic services return Result<_, tonic::Status> (176 bytes), which
// trips clippy >=1.98's result_large_err. The code is regenerated every build,
// so boxing isn't ours to do — allow it for the whole generated module.
#[allow(clippy::result_large_err)]
pub mod v5 {
    tonic::include_proto!("gitterm.agent.v5");
}
