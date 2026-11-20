use std::{process::Stdio, time::Duration};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpStream, process::Command, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

async fn request(
    socket: &mut WebSocketStream<TcpStream>,
    id: &str,
    op: &str,
    params: Value,
) -> Value {
    socket
        .send(Message::Text(
            json!({"type":"call", "id":id, "op":op, "args":params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(30), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    let message: Value = serde_json::from_str(&text).unwrap();
                    if message["id"] == id {
                        assert_eq!(message["type"], "result");
                        return message;
                    }
                }
                Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
                Message::Close(_) => panic!("native client disconnected"),
                _ => {}
            }
        }
    })
    .await
    .expect("native request timed out")
}

#[tokio::test]
#[allow(clippy::result_large_err)] // Tungstenite's handshake callback fixes this error type.
async fn native_link_executes_platform_shell() {
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/link?name=fixture", listener.local_addr().unwrap());
    // A local-only fixture credential, never registered with the public gateway.
    let credential = format!("MiXeD-{}", rand::random::<u128>());
    let mut child = Command::new(env!("CARGO_BIN_EXE_connector-client"))
        .args(["--url", &endpoint, "--token", &credential])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = timeout(Duration::from_secs(20), listener.accept())
        .await
        .expect("native link did not connect")
        .unwrap();
    let mut socket = tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.uri().path(), "/link");
            assert_eq!(request.uri().query(), Some("name=fixture"));
            assert!(request.headers()["Authorization"] == format!("Bearer {credential}"));
            let protocol = "sayuri-connector-v1";
            assert_eq!(request.headers()["Sec-WebSocket-Protocol"], protocol);
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", protocol.parse().unwrap());
            Ok(response)
        },
    )
    .await
    .unwrap();
    let ready = timeout(Duration::from_secs(20), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(ready.to_text().unwrap()).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["system"], std::env::consts::OS);
    assert_eq!(ready["shell"], if cfg!(windows) { "pwsh" } else { "bash" });
    assert_eq!(
        ready["notes"],
        json!(home.path().join(".connector").join("NOTES.md"))
    );
    let command = if cfg!(windows) {
        "[Console]::Out.Write('linked-client'); exit 7"
    } else {
        "printf linked-client; exit 7"
    };
    let run = request(&mut socket, "2", "exec", json!({"command":command})).await;
    assert_eq!(
        run["result"],
        json!({"output":"linked-client","exit_code":7})
    );
    let input = "literal $HOME; (not a command)\n";
    let command = if cfg!(windows) {
        "[Console]::Out.Write([Console]::In.ReadToEnd())"
    } else {
        "cat"
    };
    let run = request(
        &mut socket,
        "3",
        "exec",
        json!({"command":command,"stdin":input,"cwd":home.path()}),
    )
    .await;
    assert_eq!(run["result"]["output"], input);
    let command = if cfg!(windows) {
        "[Console]::Out.Write('x' * 9000000)"
    } else {
        "head -c 9000000 /dev/zero"
    };
    let overflow = request(&mut socket, "6", "exec", json!({"command":command})).await;
    assert!(overflow["error"].as_str().unwrap().contains("8 MiB"));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
