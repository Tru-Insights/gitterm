//! Bridge observations from the existing transport dispatcher, no listener.
use super::*;
use serde_json::json;

#[tokio::test]
async fn stage45_mcp_dispatch_order_caller_ack_and_errors() {
    let (commands, mut receiver) = mpsc::unbounded_channel();
    let dispatch = tokio::spawn({
        let commands = commands.clone();
        async move {
            dispatch_operation(
                &commands,
                TaskControlOperation::Get(GetTaskRequest {
                    task_id: "fixture-task".into(),
                }),
                Some("fixture-caller".into()),
            )
            .await
        }
    });
    let envelope = receiver.recv().await.unwrap();
    let mut trace = vec![
        json!({"point":"received", "operation":format!("{:?}",envelope.operation),"caller":envelope.caller,"ack_pending":!dispatch.is_finished()}),
    ];
    envelope
        .reply
        .send(Ok(json!({"task_id":"fixture-task","saved":true})));
    // Reply clones share one slot: the first acknowledgement wins.
    envelope
        .reply
        .clone()
        .send(Err("must not replace the acknowledgement".into()));
    trace.push(json!({"point":"ack", "value":dispatch.await.unwrap().unwrap()}));
    let closed_response = tokio::spawn({
        let commands = commands.clone();
        async move { dispatch_operation(&commands, TaskControlOperation::PlanLimits, None).await }
    });
    drop(receiver.recv().await.unwrap());
    trace.push(
        json!({"point":"response dropped","error":closed_response.await.unwrap().unwrap_err()}),
    );
    drop(receiver);
    trace.push(json!({"point":"bridge closed","error":dispatch_operation(&commands,TaskControlOperation::PlanLimits,None).await.unwrap_err()}));
    let (sender, mut requests) = mpsc::unbounded_channel();
    let tools = TaskMcpTools::new(sender);
    let (parts, _) = Request::builder()
        .uri("/mcp")
        .body(())
        .unwrap()
        .into_parts();
    let result = tools
        .dispatch_for_caller("review_request", TaskControlOperation::PlanLimits, &parts)
        .await;
    trace.push(json!({"point":"missing caller","is_error":result.is_error,"forwarded":requests.try_recv().is_ok(),"content":serde_json::to_value(result.content).unwrap()}));
    trace.push(json!({"point":"timeout configuration","seconds":COMMAND_TIMEOUT.as_secs()}));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stage45/tasks/mcp.trace.json");
    let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let actual = json!(trace);
    assert_eq!(actual, expected);
}
