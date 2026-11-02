//! End-to-end: POST hooks over HTTP, read them back over REST and WebSocket.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use hooktop_server::store::Store;
use hooktop_server::{AppState, Hub, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

fn state(token: Option<&str>) -> AppState {
    AppState::new(
        Arc::new(Hub::new(None)),
        None,
        token.map(hooktop_server::auth::Tokens::fixed),
    )
}

fn hook(body: Value) -> Request<Body> {
    Request::post("/hook")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn json_of(res: axum::response::Response) -> Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn session_events(sid: &str) -> Vec<Value> {
    vec![
        json!({"session_id": sid, "hook_event_name": "UserPromptSubmit", "prompt": "Rename the module"}),
        json!({"session_id": sid, "hook_event_name": "PreToolUse", "tool_name": "Grep", "tool_use_id": "t1", "tool_input": {"pattern": "old_mod"}}),
        json!({"session_id": sid, "hook_event_name": "PostToolUse", "tool_name": "Grep", "tool_use_id": "t1", "tool_response": "a.rs:1"}),
        json!({"session_id": sid, "hook_event_name": "PostToolBatch"}),
        json!({"session_id": sid, "hook_event_name": "Stop", "last_assistant_message": "Done"}),
    ]
}

#[tokio::test]
async fn hook_then_rest() {
    let st = state(None);
    let app = router(st.clone());
    for e in session_events("s1") {
        let res = app.clone().oneshot(hook(e)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await, json!({}));
    }

    let list = json_of(
        app.clone()
            .oneshot(Request::get("/api/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list[0]["session_id"], "s1");
    assert_eq!(list[0]["title"], "Rename the module");
    assert_eq!(list[0]["status"], "idle");
    assert_eq!(list[0]["tool_count"], 1);

    let snap = json_of(
        app.clone()
            .oneshot(Request::get("/api/sessions/s1").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(snap["seq"], 5);
    let tasks = snap["graph"]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[1]["title"], "Grep");
    assert_eq!(tasks[1]["tools"][0]["target"], "\"old_mod\"");

    let missing = app
        .clone()
        .oneshot(Request::get("/api/sessions/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let bad = app.oneshot(hook(json!({"nope": true}))).await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

async fn status(app: &axum::Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn every_route_needs_the_token() {
    let app = router(state(Some("sekret")));
    let ev = json!({"session_id": "s", "hook_event_name": "Stop"});
    let with = |mut r: Request<Body>, token: &str| {
        r.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        r
    };
    assert_eq!(status(&app, hook(ev.clone())).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        status(&app, with(hook(ev.clone()), "wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(status(&app, with(hook(ev.clone()), "sekret")).await, StatusCode::OK);
    for path in ["/api/health", "/api/sessions", "/api/sessions/s", "/ws"] {
        let req = Request::get(path).body(Body::empty()).unwrap();
        assert_eq!(status(&app, req).await, StatusCode::UNAUTHORIZED, "{path}");
    }
    let req = with(Request::get("/api/sessions").body(Body::empty()).unwrap(), "sekret");
    assert_eq!(status(&app, req).await, StatusCode::OK);
}

#[tokio::test]
async fn browsers_and_rebinding_are_refused_even_with_the_token() {
    let app = router(state(Some("sekret")));
    let ev = json!({"session_id": "s", "hook_event_name": "Stop"});
    let mut from_page = hook(ev.clone());
    from_page
        .headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    from_page
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert_eq!(status(&app, from_page).await, StatusCode::FORBIDDEN);

    let mut rebound = Request::get("/api/sessions").body(Body::empty()).unwrap();
    rebound
        .headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    rebound
        .headers_mut()
        .insert("host", "evil.example:4317".parse().unwrap());
    assert_eq!(status(&app, rebound).await, StatusCode::MISDIRECTED_REQUEST);

    let mut plain = Request::post("/hook")
        .header("content-type", "text/plain")
        .body(Body::from(ev.to_string()))
        .unwrap();
    plain
        .headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    assert_eq!(status(&app, plain).await, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let mut local = Request::get("/api/health").body(Body::empty()).unwrap();
    local
        .headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    local.headers_mut().insert("host", "localhost:4317".parse().unwrap());
    assert_eq!(status(&app, local).await, StatusCode::OK);
}

#[tokio::test]
async fn websocket_needs_the_token_and_no_origin() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, router(state(Some("sekret")))).into_future());
    let url = format!("ws://{addr}/ws");

    assert!(
        tokio_tungstenite::connect_async(url.as_str()).await.is_err(),
        "no token"
    );

    let mut ok = url.as_str().into_client_request().unwrap();
    ok.headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(ok).await.is_ok());

    let mut page = url.as_str().into_client_request().unwrap();
    page.headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    page.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(page).await.is_err(),
        "cross-site WebSocket"
    );
}

#[tokio::test]
async fn events_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("hooktop-store-test-{}", std::process::id()));
    let db = dir.join("events.db");
    {
        let (store, existing) = Store::open(&db).unwrap();
        assert!(existing.is_empty());
        let st = AppState::new(Arc::new(Hub::new(Some(store.writer.clone()))), Some(db.clone()), None);
        let app = router(st);
        for e in session_events("persisted") {
            app.clone().oneshot(hook(e)).await.unwrap();
        }
        store.writer.shutdown(Duration::from_secs(2));

        let res = app
            .oneshot(
                Request::get("/api/sessions/persisted/events?after=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let rows = json_of(res).await;
        assert_eq!(rows.as_array().unwrap().len(), 3);
        assert_eq!(rows[0]["hook"], "PostToolUse");
    }
    let (_store, existing) = Store::open(&db).unwrap();
    assert_eq!(existing.len(), 5);
    let hub = Hub::new(None);
    hub.restore(existing);
    let (_, g) = hub.snapshot("persisted").unwrap();
    assert_eq!(g.tasks.len(), 2);
    // New events continue after the restored seq.
    assert_eq!(
        hub.ingest(
            serde_json::from_value(json!({"session_id": "persisted", "hook_event_name": "Stop"})).unwrap(),
            0,
            false
        ),
        6
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn websocket_streams_snapshot_then_deltas() {
    let st = state(None);
    let hub = st.hub.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, router(st)).into_future());

    let first: Vec<_> = session_events("live").into_iter().take(2).collect();
    for e in &first {
        hub.ingest(serde_json::from_value(e.clone()).unwrap(), 0, false);
    }

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?session=live"))
        .await
        .unwrap();
    let mut next = async || -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    };

    let hello = next().await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["sessions"][0]["session_id"], "live");
    let snap = next().await;
    assert_eq!(snap["type"], "snapshot");
    assert_eq!(snap["seq"], 2);
    assert_eq!(snap["graph"]["tasks"].as_array().unwrap().len(), 2);

    // Three more events. They usually share one 16 ms flush, but on a busy
    // machine they may be split across frames, so read frames until the last
    // seq arrives, checking seqs only ever increase.
    for e in session_events("live").into_iter().skip(2) {
        hub.ingest(serde_json::from_value(e).unwrap(), 0, false);
    }
    let mut ops: Vec<String> = Vec::new();
    let mut last_seq = 2;
    for _ in 0..10 {
        let m = next().await;
        if m["type"] != "deltas" {
            continue;
        }
        let seq = m["seq"].as_u64().unwrap();
        assert!(seq > last_seq, "deltas arrive in seq order");
        last_seq = seq;
        ops.extend(
            m["deltas"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["op"].as_str().unwrap().to_string()),
        );
        if seq == 5 {
            break;
        }
    }
    assert_eq!(last_seq, 5, "all three events were delivered");
    assert!(ops.iter().any(|o| o == "tool") && ops.iter().any(|o| o == "session"));

    // Switching subscription yields a snapshot of the other session.
    hub.ingest(
        serde_json::from_value(json!({"session_id": "other", "hook_event_name": "UserPromptSubmit", "prompt": "hi"}))
            .unwrap(),
        0,
        false,
    );
    ws.send(Message::Text(
        json!({"type": "subscribe", "session_id": "other"}).to_string().into(),
    ))
    .await
    .unwrap();
    let mut saw_snapshot = false;
    for _ in 0..4 {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "snapshot" {
                assert_eq!(v["session_id"], "other");
                saw_snapshot = true;
                break;
            }
        }
    }
    assert!(saw_snapshot);
}

#[tokio::test]
async fn unknown_paths_are_not_found() {
    let app = router(state(None));
    for path in ["/", "/api/nope"] {
        let res = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn an_unused_server_stops_but_an_open_window_keeps_it_up() {
    use hooktop_server::server::{ServerConfig, start};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let tokens = hooktop_server::auth::Tokens::fixed("idle-test-token-123456");
    let cfg = |port| ServerConfig {
        port,
        no_store: true,
        token: Some(tokens.clone()),
        idle_timeout: Some(Duration::from_millis(400)),
        ..ServerConfig::default()
    };

    // Nothing uses it: it says it's idle.
    let server = start(cfg(0)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), server.idle())
        .await
        .expect("stops when unused");
    server.stop().await.unwrap();

    // An open WebSocket counts as use; refused requests don't.
    let server = start(cfg(0)).await.unwrap();
    let mut req = format!("ws://{}/ws", server.addr).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer idle-test-token-123456".parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), server.idle())
            .await
            .is_err(),
        "an open window keeps hooktop running"
    );
    drop(ws);
    let addr = server.addr;
    let probe = tokio::spawn(async move {
        // Unauthenticated pokes every 100 ms shouldn't keep it alive.
        for _ in 0..30 {
            if let Ok(mut s) = tokio::net::TcpStream::connect(addr).await {
                use tokio::io::AsyncWriteExt;
                let _ = s
                    .write_all(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(3), server.idle())
        .await
        .expect("stops once the window closes");
    probe.abort();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn token_usage_is_read_from_the_transcript() {
    // A fake Claude config folder holding a transcript.
    let dir = std::env::temp_dir().join(format!("hooktop-api-transcript-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("projects")).unwrap();
    let transcript = dir.join("projects").join("s.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"user","message":{"content":"the prompt text"}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-11-02T13:00:00Z","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}],"usage":{"input_tokens":1000000,"output_tokens":100000}}}"#, "\n",
        ),
    )
    .unwrap();
    // SAFETY: no other test in this binary reads CLAUDE_CONFIG_DIR.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

    let st = state(None);
    let hub = st.hub.clone();
    let app = router(st);
    let tp = transcript.to_str().unwrap();
    for ev in [
        json!({"session_id": "u", "hook_event_name": "UserPromptSubmit", "prompt": "go", "transcript_path": tp}),
        json!({"session_id": "u", "hook_event_name": "PreToolUse", "tool_name": "Read", "tool_use_id": "t1", "tool_input": {}, "transcript_path": tp}),
        json!({"session_id": "u", "hook_event_name": "PostToolBatch", "transcript_path": tp}),
        // A transcript outside the config folder is ignored.
        json!({"session_id": "u", "hook_event_name": "Stop", "transcript_path": "/etc/hosts"}),
    ] {
        assert_eq!(app.clone().oneshot(hook(ev)).await.unwrap().status(), StatusCode::OK);
    }
    // Reading happens off the request path.
    let mut usage = Vec::new();
    for _ in 0..50 {
        usage = hub.snapshot("u").unwrap().1.usage;
        if !usage.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(usage.len(), 1);
    let g = hub.snapshot("u").unwrap().1;
    let turn = g
        .tasks
        .iter()
        .find(|t| t.header.kind == hooktop_core::TaskKind::Turn)
        .unwrap()
        .header
        .id;
    assert_eq!(usage[0].task, Some(turn), "matched by tool call");
    let c = hooktop_core::stats::costs(&g, &hooktop_core::pricing::Pricing::builtin());
    assert!((c.total - (4.0 + 2.0)).abs() < 1e-9, "1M in at $4 + 100k out at $20");
    let stored = serde_json::to_string(&g.usage).unwrap();
    assert!(!stored.contains("prompt text"), "only numbers are kept");

    // Re-reading the same lines changes nothing.
    assert!(!hub.ingest_usage("u", usage[0].clone()));
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    std::fs::remove_dir_all(dir).unwrap();
}
