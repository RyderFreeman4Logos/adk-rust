//! Custom graph execution must not turn a yielded agent failure into node success.

use adk_core::{AdkError, Agent, Content, Event, EventStream, InvocationContext};
use adk_graph::GraphError;
use adk_graph::checkpoint::{Checkpointer, MemoryCheckpointer};
use adk_graph::edge::{END, START};
use adk_graph::graph::StateGraph;
use adk_graph::node::{AgentNode, ExecutionConfig, NodeOutput};
use adk_graph::state::State;
use adk_graph::stream::{StreamEvent, StreamMode};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const TERMINAL: &str = "synthetic-terminal-agent-stream-error";

struct ScriptedAgent {
    script: Vec<Result<&'static str, &'static str>>,
    counters: Arc<[AtomicUsize; 5]>,
}

#[async_trait]
impl Agent for ScriptedAgent {
    fn name(&self) -> &str {
        "worker"
    }

    fn description(&self) -> &str {
        "emits the supplied event results"
    }

    fn sub_agents(&self) -> &[Arc<dyn Agent>] {
        &[]
    }

    async fn run(&self, _ctx: Arc<dyn InvocationContext>) -> adk_core::Result<EventStream> {
        self.counters[0].fetch_add(1, Ordering::SeqCst);
        let counters = Arc::clone(&self.counters);
        let stream = futures::stream::iter(self.script.clone()).map(move |item| {
            counters[1].fetch_add(1, Ordering::SeqCst);
            item.map(|text| {
                let mut event = Event::new("scripted-invocation");
                event.set_content(Content::new("assistant").with_text(text));
                event
            })
            .map_err(AdkError::agent)
        });
        Ok(Box::pin(stream))
    }
}

fn texts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(Event::content)
        .flat_map(|content| content.parts.iter().filter_map(|part| part.text()))
        .map(str::to_string)
        .collect()
}

async fn run_custom_graph(
    script: Vec<Result<&'static str, &'static str>>,
) -> (Vec<adk_graph::Result<StreamEvent>>, [usize; 5], Option<State>) {
    // Agent runs, polled items, output mapper, goto mapper, successor runs.
    let counters = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let output_counters = Arc::clone(&counters);
    let goto_counters = Arc::clone(&counters);
    let successor_counters = Arc::clone(&counters);
    let checkpointer = Arc::new(MemoryCheckpointer::new());
    let graph = StateGraph::with_channels(&["seed", "observed", "successor"])
        .add_node(
            AgentNode::new(Arc::new(ScriptedAgent { script, counters: Arc::clone(&counters) }))
                .with_output_mapper(move |events| {
                    output_counters[2].fetch_add(1, Ordering::SeqCst);
                    HashMap::from([("observed".to_string(), json!(texts(events)))])
                })
                .with_goto_mapper(move |_updates| {
                    goto_counters[3].fetch_add(1, Ordering::SeqCst);
                    Some(vec!["successor".to_string()])
                }),
        )
        .add_node_fn("successor", move |_ctx| {
            successor_counters[4].fetch_add(1, Ordering::SeqCst);
            async { Ok(NodeOutput::new().with_update("successor", json!(true))) }
        })
        .add_edge(START, "worker")
        .add_edge("worker", "successor")
        .add_edge("successor", END)
        .compile()
        .expect("compile graph")
        .with_checkpointer_arc(checkpointer as Arc<dyn Checkpointer>);
    let input = State::from([("seed".to_string(), json!("unchanged"))]);
    let events = graph
        .stream(input, ExecutionConfig::new("agent-stream-error"), StreamMode::Custom)
        .collect()
        .await;
    let saved = graph.get_state("agent-stream-error").await.expect("read checkpoint");
    (events, std::array::from_fn(|index| counters[index].load(Ordering::SeqCst)), saved)
}

fn assert_terminal_failure(events: &[adk_graph::Result<StreamEvent>]) {
    assert!(
        matches!(events.first(), Some(Ok(StreamEvent::NodeStart { node, step: 0 })) if node == "worker")
    );
    let failures: Vec<_> = events.iter().filter_map(|event| event.as_ref().err()).collect();
    assert_eq!(failures.len(), 1, "the graph must publish one terminal error: {events:?}");
    match failures[0] {
        GraphError::NodeExecutionFailed { node, message } => {
            assert_eq!(node, "worker");
            assert!(
                message.contains(TERMINAL),
                "the original error marker must survive: {message}"
            );
            assert!(!message.contains("later-error"), "the first error must win: {message}");
        }
        other => panic!("expected NodeExecutionFailed, got {other:?}"),
    }
    // Custom mode buffers a node's successful events until the node succeeds.
    assert_eq!(events.len(), 2, "a failed node must publish no partial events or Done: {events:?}");
    assert!(events.last().is_some_and(Result::is_err));
}

#[tokio::test]
async fn custom_graph_preserves_an_error_only_agent_stream() {
    let (events, counters, saved) = run_custom_graph(vec![Err(TERMINAL)]).await;
    assert_eq!(counters, [1, 1, 0, 0, 0], "failure must stop before mappers and successor");
    assert_terminal_failure(&events);
    assert!(saved.is_none(), "a failed node must not create a successful checkpoint");
}

#[tokio::test]
async fn custom_graph_discards_partial_success_and_stops_at_the_first_error() {
    let (events, counters, saved) = run_custom_graph(vec![
        Ok("prefix"),
        Err(TERMINAL),
        Ok("must-not-be-polled"),
        Err("later-error"),
    ])
    .await;
    assert_eq!(
        counters,
        [1, 2, 0, 0, 0],
        "first failure must stop polling and all success effects"
    );
    assert_terminal_failure(&events);
    assert!(saved.is_none(), "partial agent output must not be committed");
}

#[tokio::test]
async fn custom_graph_preserves_successful_event_order_mapping_and_routing() {
    let (events, counters, saved) = run_custom_graph(vec![Ok("first"), Ok("second")]).await;
    assert_eq!(counters, [1, 2, 1, 1, 1]);
    assert!(events.iter().all(Result::is_ok), "successful stream: {events:?}");
    let agent_events: Vec<Event> = events
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::Custom { node, event_type, data })
                if node == "worker" && event_type == "agent_event" =>
            {
                Some(serde_json::from_value(data.clone()).expect("serialized agent event"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts(&agent_events), ["first", "second"]);
    let expected = State::from([
        ("seed".to_string(), json!("unchanged")),
        ("observed".to_string(), json!(["first", "second"])),
        ("successor".to_string(), json!(true)),
    ]);
    assert!(
        matches!(events.last(), Some(Ok(StreamEvent::Done { state, .. })) if state == &expected)
    );
    assert_eq!(saved, Some(expected));
}
