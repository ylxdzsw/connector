use std::{process::Stdio, time::Duration};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpStream, process::Command, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

async fn request(
    socket: &mut WebSocketStream<TcpStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
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
                        assert!(message.get("error").is_none(), "native protocol error");
                        return message["result"].clone();
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
async fn native_link_executes_platform_shell_and_patches() {
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    // A local-only fixture credential, never registered with the public gateway.
    let credential = format!("MiXeD-{}", rand::random::<u128>());
    let mut child = Command::new(env!("CARGO_BIN_EXE_connector-client"))
        .args(["--gateway", &gateway, "--token", &credential])
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
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            assert_eq!(request.uri().path(), "/link");
            assert!(request.headers()["Authorization"] == format!("Bearer {credential}"));
            Ok(response)
        },
    )
    .await
    .unwrap();
    let info = request(
        &mut socket,
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"native-smoke","version":"1"}}),
    )
    .await;
    let environment = &info["_meta"]["com.ylxdzsw.connector/client-environment"];
    assert_eq!(environment["system"], std::env::consts::OS);
    assert_eq!(
        environment["shell"],
        if cfg!(windows) { "pwsh" } else { "bash" }
    );
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let command = if cfg!(windows) {
        "[Console]::Out.Write('linked-client'); exit 7"
    } else {
        "printf linked-client; exit 7"
    };
    let run = request(
        &mut socket,
        2,
        "tools/call",
        json!({"name":"run","arguments":{"command":command}}),
    )
    .await;
    assert_eq!(
        run["structuredContent"],
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
        3,
        "tools/call",
        json!({"name":"run","arguments":{"command":command,"stdin":input,"cwd":home.path()}}),
    )
    .await;
    assert_eq!(run["structuredContent"]["output"], input);
    let patch = request(&mut socket, 4, "tools/call", json!({"name":"apply_patch","arguments":{"cwd":home.path(),"patch":"*** Begin Patch\n*** Add File: smoke.txt\n+native-patch\n*** End Patch"}})).await;
    assert_ne!(patch["isError"], true);
    assert_eq!(
        std::fs::read_to_string(home.path().join("smoke.txt")).unwrap(),
        "native-patch\n"
    );
    let notes = request(
        &mut socket,
        5,
        "tools/call",
        json!({"name":"notes_metadata","arguments":{}}),
    )
    .await;
    assert_eq!(
        notes["structuredContent"]["path"],
        json!(home.path().join(".connector").join("NOTES.md"))
    );
    let command = if cfg!(windows) {
        "[Console]::Out.Write('x' * 9000000)"
    } else {
        "head -c 9000000 /dev/zero"
    };
    let overflow = request(
        &mut socket,
        6,
        "tools/call",
        json!({"name":"run","arguments":{"command":command}}),
    )
    .await;
    assert_eq!(overflow["isError"], true);
    assert!(
        overflow["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("8 MiB")
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
