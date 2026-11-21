//! Compatibility facade: the task MCP server lives in
//! `gitterm_core::task_mcp`, so both apps can host it. Root callers keep
//! this path.
pub use gitterm_core::task_mcp::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// The facade and core reach the same process-wide ingress: one
    /// receiver, taken once, fed in order by senders from either path. No
    /// other test in this binary takes the receiver.
    #[tokio::test]
    async fn root_facade_and_core_share_one_ingress() {
        let core_sender = gitterm_core::task_mcp::command_sender();
        let root_sender = command_sender();
        let mut receiver = take_command_receiver().expect("the first take gets the receiver");
        assert!(gitterm_core::task_mcp::take_command_receiver().is_none());
        assert!(take_command_receiver().is_none());

        let first = tokio::spawn(async move {
            core_sender
                .dispatch(TaskControlOperation::PlanLimits, Some("core".into()))
                .await
        });
        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.caller.as_deref(), Some("core"));
        envelope.reply.send(Ok(serde_json::json!("core")));
        assert_eq!(first.await.unwrap(), Ok(serde_json::json!("core")));

        let second = tokio::spawn(async move {
            root_sender
                .dispatch(TaskControlOperation::MergePlanGet, Some("root".into()))
                .await
        });
        let envelope = receiver.recv().await.unwrap();
        assert!(matches!(
            envelope.operation,
            TaskControlOperation::MergePlanGet
        ));
        assert_eq!(envelope.caller.as_deref(), Some("root"));
        envelope.reply.send(Ok(serde_json::json!("root")));
        assert_eq!(second.await.unwrap(), Ok(serde_json::json!("root")));
    }
}
