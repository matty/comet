//! Fake Codex app-server that completes setup, then scripts `turn/start`
//! failures. This is D45's missing turn-start-stall primitive, separate from
//! the thread-setup stall that never creates a thread.

use std::io::{BufRead, StdinLock, Write};
use std::time::Duration;

fn rid(line: &str) -> String {
    match line.rfind("\"id\":") {
        Some(at) => line[at + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect(),
        None => String::new(),
    }
}

fn reply(id: &str, result: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, r#"{{"id":{id},"result":{result}}}"#);
    let _ = out.flush();
}

fn reply_error(id: &str, message: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(
        out,
        r#"{{"id":{id},"error":{{"code":-32603,"message":"{message}"}}}}"#
    );
    let _ = out.flush();
}

fn read_request(stdin: &mut StdinLock<'_>, expected_method: &str) -> String {
    let mut line = String::new();
    if stdin.read_line(&mut line).unwrap_or_default() == 0 {
        std::process::exit(2);
    }
    let method = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|value| value["method"].as_str().map(str::to_owned));
    if method.as_deref() != Some(expected_method) {
        eprintln!(
            "D134_UNEXPECTED_REQUEST expected={expected_method} got={method:?} line={line:?}"
        );
        std::process::exit(2);
    }
    line
}

fn prompt(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value["params"]["input"]
                .as_array()
                .and_then(|input| input.first())
                .and_then(|item| item["text"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn record_pid(prompt: &str) {
    let Some((_, path)) = prompt.split_once('|') else {
        eprintln!("D134_PID_FILE_MISSING prompt={prompt:?}");
        std::process::exit(2);
    };
    if let Err(error) = std::fs::write(path, std::process::id().to_string()) {
        eprintln!("D134_PID_FILE_WRITE_FAILED path={path:?} error={error}");
        std::process::exit(2);
    }
}

fn stall(prompt: &str) {
    record_pid(prompt);
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "D134_TURN_START_STALL_PRIVATE_DIAGNOSTIC");
    let _ = stderr.flush();
    drop(stderr);
    std::thread::sleep(Duration::from_secs(30));
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    let initialize = read_request(&mut stdin, "initialize");
    reply(&rid(&initialize), r#"{"userAgent":"fake-codex"}"#);
    let _initialized = read_request(&mut stdin, "initialized");
    let thread_start = read_request(&mut stdin, "thread/start");
    reply(&rid(&thread_start), r#"{"thread":{"id":"th-1"}}"#);

    let turn_start = read_request(&mut stdin, "turn/start");
    let initial_prompt = prompt(&turn_start);
    if initial_prompt.starts_with("scenario:turn-start-private-error") {
        reply_error(&rid(&turn_start), "D134_PRIVATE_PROVIDER_ERROR");
    } else if initial_prompt.starts_with("scenario:turn-start-stall|") {
        stall(&initial_prompt);
    } else if initial_prompt.starts_with("scenario:steer-fallback-turn-start-") {
        reply(&rid(&turn_start), r#"{"turn":{"id":"t-1"}}"#);
        let mut out = std::io::stdout();
        let _ = out
            .write_all(b"{\"method\":\"turn/started\",\"params\":{\"turn\":{\"id\":\"t-1\"}}}\n");
        let _ = out
            .write_all(b"{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"id\":\"t-1\"}}}\n");
        let _ = out.flush();

        let fallback = read_request(&mut stdin, "turn/start");
        let fallback_prompt = prompt(&fallback);
        if fallback_prompt.starts_with("scenario:turn-start-private-error") {
            reply_error(&rid(&fallback), "D134_PRIVATE_PROVIDER_ERROR");
        } else {
            stall(&fallback_prompt);
        }
    } else {
        eprintln!("D134_UNKNOWN_SCENARIO prompt={initial_prompt:?}");
        std::process::exit(2);
    }
}
