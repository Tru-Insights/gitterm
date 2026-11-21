//! Bridge observations from the existing transport dispatcher, no listener.
use super::*;
use serde_json::json;

// These synthetic responses characterize transport wrapping, not application
// persistence. launch-handoff.trace.json separately exercises the real owner.
#[tokio::test]
async fn stage45_task_tools_wire_and_batch_order() {
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../../tests/fixtures/stage45/tasks/tools.wire.json"
    ))
    .unwrap();
    for row in rows {
        let (commands, mut receiver) = command_channel();
        let tool = TaskMcpTools::new(commands, refused_html_preview());
        let expected = row.clone();
        let bridge = tokio::spawn(async move {
            let envelope = receiver.recv().await.unwrap();
            assert_eq!(format!("{:?}", envelope.operation), expected["operation"]);
            assert_eq!(envelope.caller.as_deref(), Some("fixture-caller"));
            envelope.reply.send(Ok(expected["reply"].clone()));
        });
        let (parts, _) = Request::builder()
            .uri("/mcp?caller=fixture-caller")
            .body(())
            .unwrap()
            .into_parts();
        let args = row["request"].clone();
        let result = match row["tool"].as_str().unwrap() {
            "task_list" => {
                tool.task_list(
                    Parameters(serde_json::from_value(args).unwrap()),
                    Extension(parts),
                )
                .await
            }
            "task_get" => {
                tool.task_get(
                    Parameters(serde_json::from_value(args).unwrap()),
                    Extension(parts),
                )
                .await
            }
            "task_create" => {
                tool.task_create(
                    Parameters(serde_json::from_value(args).unwrap()),
                    Extension(parts),
                )
                .await
            }
            "task_launch_session" => {
                tool.task_launch_session(
                    Parameters(serde_json::from_value(args).unwrap()),
                    Extension(parts),
                )
                .await
            }
            "task_update_handoff" => {
                tool.task_update_handoff(
                    Parameters(serde_json::from_value(args).unwrap()),
                    Extension(parts),
                )
                .await
            }
            other => panic!("unhandled task tool {other}"),
        };
        bridge.await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(result.structured_content, Some(row["reply"].clone()));
    }

    let (commands, mut receiver) = command_channel();
    let tool = TaskMcpTools::new(commands, refused_html_preview());
    let bridge = tokio::spawn(async move {
        let first = receiver.recv().await.unwrap();
        assert!(receiver.try_recv().is_err(), "batch waits for each reply");
        first.reply.send(Err("synthetic save failure".into()));
        let second = receiver.recv().await.unwrap();
        second.reply.send(Ok(json!({"task_id":"second"})));
    });
    let (parts, _) = Request::builder()
        .uri("/mcp")
        .body(())
        .unwrap()
        .into_parts();
    let result = tool
        .task_create_batch(
            Parameters(
                serde_json::from_value(json!({"tasks":[
                    {"title":"first","objective":"one","repository_path":"/fixture/repo"},
                    {"title":"second","objective":"two","repository_path":"/fixture/repo"}
                ]}))
                .unwrap(),
            ),
            Extension(parts),
        )
        .await;
    bridge.await.unwrap();
    assert_eq!(
        result.structured_content,
        Some(json!({"results":[
            {"title":"first","ok":false,"error":"synthetic save failure"},
            {"title":"second","ok":true,"task":{"task_id":"second"}}
        ]}))
    );
}

#[tokio::test]
async fn stage45_mcp_dispatch_order_caller_ack_and_errors() {
    let (commands, mut receiver) = command_channel();
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
    let (sender, mut requests) = command_channel();
    let tools = TaskMcpTools::new(sender, refused_html_preview());
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
