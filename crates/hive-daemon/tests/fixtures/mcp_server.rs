//! Independent JSON-RPC fixture: using another SDK peer would hide wire mistakes.

use serde_json::{Value, json};
use std::{fs::OpenOptions, io::Write};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn record(path: &str, event: Value) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{event}").unwrap();
}

async fn send(output: &mut tokio::io::Stdout, id: Value, result: Value) {
    output
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    output.flush().await.unwrap();
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    let log = &args[1];
    let mode = args.get(2).map(String::as_str).unwrap_or("normal");
    record(log, json!({"event": "spawn", "pid": std::process::id()}));
    if mode == "pipe-holder" {
        std::fs::write(format!("{log}.child"), std::process::id().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if mode == "descendant" {
        // Keep protocol pipes open after leader exit to exercise independent exit supervision.
        let _child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([log, "pipe-holder"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        while !std::path::Path::new(&format!("{log}.child")).exists() {
            std::thread::yield_now();
        }
        std::process::exit(8);
    }
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut held = Vec::new();
    loop {
        let line = tokio::select! {
            line = input.next_line() => line.unwrap(),
            _ = tokio::time::sleep(std::time::Duration::from_millis(5)), if !held.is_empty() => {
                // Out-of-band completion lets tests close every session without adding a lease.
                if std::path::Path::new(&format!("{log}.release")).exists() {
                    while let Some((id, value)) = held.pop() { send(&mut output, id, value).await; }
                }
                continue;
            }
        };
        let Some(line) = line else {
            break;
        };
        let message: Value = serde_json::from_str(&line).unwrap();
        record(log, message.clone());
        let id = message["id"].clone();
        match message["method"].as_str().unwrap_or("") {
            "initialize" => {
                if mode == "exit" {
                    std::process::exit(7);
                }
                if mode == "hang" {
                    continue;
                }
                if mode == "oversized" {
                    output.write_all(&vec![b'x'; 2048]).await.unwrap();
                    output.write_all(b"\n").await.unwrap();
                    output.flush().await.unwrap();
                    continue;
                }
                if mode == "malformed" {
                    output.write_all(b"not json\n").await.unwrap();
                    output.flush().await.unwrap();
                    continue;
                }
                if mode == "stderr" {
                    tokio::io::stderr()
                        .write_all(&vec![b'x'; 1024 * 1024])
                        .await
                        .unwrap();
                }
                let version = if mode == "version" {
                    "unsupported"
                } else {
                    "2025-11-25"
                };
                send(&mut output, id, json!({"protocolVersion": version, "serverInfo": {"name": "fixture", "version": "1"}, "capabilities": {"tools": {}}})).await;
            }
            "ping" => send(&mut output, id, json!({})).await,
            "notifications/initialized" if mode == "roots" => {
                output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"root-query\",\"method\":\"roots/list\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":\"sampling-query\",\"method\":\"sampling/createMessage\",\"params\":{}}\n").await.unwrap();
                output.flush().await.unwrap();
            }
            #[cfg(unix)]
            "notifications/initialized" if mode == "eof" => {
                use std::os::unix::process::CommandExt;
                // Exec preserves the leader PID but closes its protocol stdout and stdin.
                let error = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([log, "pipe-holder"])
                    .stdout(std::process::Stdio::null())
                    .stdin(std::process::Stdio::null())
                    .exec();
                panic!("fixture exec failed: {error}");
            }
            "tools/call" => {
                match message["params"]["name"].as_str().unwrap_or("echo") {
                    "crash" => std::process::exit(9),
                    "error" => {
                        output.write_all(format!("{}\n", json!({"jsonrpc":"2.0", "id":id, "error":{"code":-12345,"message":"seed-secret", "data":{"token":"seed-secret"}}})).as_bytes()).await.unwrap();
                        output.flush().await.unwrap();
                    }
                    "ambiguous-response" => {
                        // Valid JSON with conflicting response arms exercises protocol validation,
                        // while keeping this process alive rules out ordinary exit detection.
                        output.write_all(format!("{}\n", json!({"jsonrpc":"2.0", "id":id, "result":{}, "error":{"code":-32603,"message":"conflicting response"}})).as_bytes()).await.unwrap();
                        output.flush().await.unwrap();
                    }
                    "hold" => held.push((id, message["params"]["arguments"].clone())),
                    "release" => {
                        // Reverse completion order exposes accidental FIFO response routing.
                        while let Some((id, value)) = held.pop() { send(&mut output, id, value).await; }
                        send(&mut output, id, json!({"released": true})).await;
                    }
                    _ => send(&mut output, id, json!({"pid": std::process::id(), "arguments": message["params"]["arguments"]})).await,
                }
            }
            _ => {}
        }
    }
    record(log, json!({"event": "eof"}));
}
