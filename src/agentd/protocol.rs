// Generated tonic services return Result<_, tonic::Status> (176 bytes), which
// trips clippy >=1.98's result_large_err, and tonic's async_trait expansion
// stacks #[must_use] on already-must_use types, which trips clippy >=1.99's
// double_must_use. The code is regenerated every build, so neither is ours
// to fix — allow both for the whole generated module.
#[allow(clippy::result_large_err, clippy::double_must_use)]
pub mod v6 {
    tonic::include_proto!("gitterm.agent.v6");
}
