//! Integration test for `quorum_difficulty`: spin up several stub `monerod`
//! HTTP servers on local ports, point the function at them, verify the
//! quorum/outlier/missing behavior over real reqwest calls.

use axum::{routing::post, Json, Router};
use pps_rate::quorum_difficulty;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
struct Sample {
    height: u64,
    difficulty: u64,
    /// If true, the stub answers 500. Used to simulate a dead node.
    dead: bool,
}

async fn spawn_node(s: Sample) -> String {
    let s = Arc::new(s);
    let app = Router::new().route(
        "/json_rpc",
        post({
            let s = s.clone();
            move |Json(_body): Json<Value>| {
                let s = s.clone();
                async move {
                    if s.dead {
                        return axum::http::Response::builder()
                            .status(500)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    let body = json!({
                        "jsonrpc":"2.0","id":"0",
                        "result": {"height": s.height, "difficulty": s.difficulty}
                    });
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}/json_rpc")
}

#[tokio::test]
async fn three_nodes_full_agreement_returns_difficulty() {
    let urls = vec![
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
    ];
    let client = reqwest::Client::new();
    let d = quorum_difficulty(&client, &urls, 2).await.unwrap();
    assert_eq!(d, 9_000);
}

#[tokio::test]
async fn one_node_dead_quorum_still_met() {
    let urls = vec![
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: true }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
    ];
    let client = reqwest::Client::new();
    let d = quorum_difficulty(&client, &urls, 2).await.unwrap();
    assert_eq!(d, 9_000);
}

#[tokio::test]
async fn malicious_outlier_ignored() {
    let urls = vec![
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
        spawn_node(Sample { height: 100, difficulty: 1, dead: false }).await, // lying — would inflate credits
    ];
    let client = reqwest::Client::new();
    let d = quorum_difficulty(&client, &urls, 2).await.unwrap();
    assert_eq!(d, 9_000, "outlier must not poison the result");
}

#[tokio::test]
async fn two_nodes_dead_no_quorum_errors() {
    let urls = vec![
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: true }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: true }).await,
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await,
    ];
    let client = reqwest::Client::new();
    let result = quorum_difficulty(&client, &urls, 2).await;
    assert!(result.is_err(), "only one live node, quorum=2 must fail");
}

#[tokio::test]
async fn prefers_newer_block_when_two_agree() {
    let urls = vec![
        spawn_node(Sample { height: 100, difficulty: 9_000, dead: false }).await, // stale
        spawn_node(Sample { height: 101, difficulty: 9_500, dead: false }).await, // fresh
        spawn_node(Sample { height: 101, difficulty: 9_500, dead: false }).await, // fresh
    ];
    let client = reqwest::Client::new();
    let d = quorum_difficulty(&client, &urls, 2).await.unwrap();
    assert_eq!(d, 9_500);
}
