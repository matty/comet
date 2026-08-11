//! Fake Codex app-server whose `model/list` never runs out of pages, handing
//! back a fresh cursor every time.
//!
//! The cursor advances, so the client's did-not-advance guard cannot fire: the
//! page cap is the only thing standing between this and a discovery that never
//! returns, while the picker awaits it.

use std::io::{BufRead, StdinLock, Write};

fn emit(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn read_line(stdin: &mut StdinLock<'_>) -> Option<String> {
    let mut buf = String::new();
    match stdin.read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn rid(line: &str) -> String {
    match line.rfind("\"id\":") {
        Some(at) => line[at + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect(),
        None => String::new(),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    let Some(init) = read_line(&mut stdin) else {
        return;
    };
    emit(&format!(
        r#"{{"id":{},"result":{{"userAgent":"fake-codex-endless"}}}}"#,
        rid(&init)
    ));
    let Some(_initialized) = read_line(&mut stdin) else {
        return;
    };

    let mut page = 0;
    while let Some(line) = read_line(&mut stdin) {
        if !line.contains(r#""method":"model/list""#) {
            return;
        }
        page += 1;
        emit(&format!(
            r#"{{"id":{},"result":{{"data":[{{"id":"gpt-page-{page}","displayName":"Page {page}","hidden":false,"supportedReasoningEfforts":[],"inputModalities":["text"]}}],"nextCursor":"{page}"}}}}"#,
            rid(&line)
        ));
    }
}
