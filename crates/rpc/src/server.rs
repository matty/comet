//! Server side: dispatch loop over string frames + the WebSocket acceptor.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as WsMessage};

use crate::{ClientFrame, RpcError, RpcReply, RpcService, ServerFrame};

/// Serve one connection: read client frames from `inbound`, write server frames to `out`.
/// Returns when `inbound` closes; all in-flight request tasks are aborted on exit.
pub async fn serve_connection(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    inbound: mpsc::Receiver<String>,
) {
    serve_connection_guarded(service, out, inbound, None).await;
}

async fn serve_connection_guarded(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    mut inbound: mpsc::Receiver<String>,
    guard: Option<ConnectionGuard>,
) {
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    'connection: while let Some(payload) = inbound.recv().await {
        // ndjson: a transport may batch several frames per message.
        for line in payload.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if guard.as_ref().is_some_and(|guard| !guard()) {
                break 'connection;
            }
            let frame: ClientFrame = match serde_json::from_str(line) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "rpc: dropping malformed client frame");
                    continue;
                }
            };
            running.retain(|_, task| !task.is_finished());
            if frame.cancel {
                if let Some(task) = running.remove(&frame.id) {
                    task.abort();
                }
                continue;
            }
            let Some(method) = frame.method else {
                tracing::warn!(id = frame.id, "rpc: frame has neither method nor cancel");
                continue;
            };
            let task = tokio::spawn(handle_request(
                service.clone(),
                out.clone(),
                frame.id,
                method,
                frame.params,
                guard.clone(),
            ));
            running.insert(frame.id, task.abort_handle());
        }
    }
    for (_, task) in running {
        task.abort();
    }
}

async fn handle_request(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    id: u64,
    method: String,
    params: serde_json::Value,
    guard: Option<ConnectionGuard>,
) {
    if guard.as_ref().is_some_and(|guard| !guard()) {
        return;
    }
    let send = |frame: ServerFrame| {
        let out = out.clone();
        async move {
            match serde_json::to_string(&frame) {
                Ok(json) => out.send(json).await.map_err(|_| RpcError::Closed),
                Err(err) => {
                    tracing::error!(error = %err, "rpc: failed to serialize server frame");
                    Err(RpcError::Closed)
                }
            }
        }
    };
    match service.handle(&method, params).await {
        Ok(RpcReply::Value(value)) => {
            let _ = send(ServerFrame {
                id,
                ok: Some(value),
                ..Default::default()
            })
            .await;
        }
        Ok(RpcReply::Stream(mut stream)) => {
            while let Some(item) = stream.next().await {
                if send(ServerFrame {
                    id,
                    item: Some(item),
                    ..Default::default()
                })
                .await
                .is_err()
                {
                    return; // connection gone
                }
            }
            let _ = send(ServerFrame {
                id,
                done: true,
                ..Default::default()
            })
            .await;
        }
        Err(err) => {
            let _ = send(ServerFrame {
                id,
                err: Some(err.to_string()),
                ..Default::default()
            })
            .await;
        }
    }
}

/// Accept WebSocket connections forever, serving each with `service`.
pub async fn serve_ws_listener(listener: TcpListener, service: Arc<dyn RpcService>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!(%peer, "rpc: connection accepted");
                tokio::spawn(serve_ws_socket(stream, service.clone()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "rpc: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_ws_socket(stream: TcpStream, service: Arc<dyn RpcService>) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(err) => {
            tracing::warn!(error = %err, "rpc: websocket handshake failed");
            return;
        }
    };
    serve_websocket(ws, service).await;
}

pub(crate) async fn serve_websocket<S>(ws: WebSocketStream<S>, service: Arc<dyn RpcService>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_websocket_guarded(ws, service, None).await;
}

pub(crate) type ConnectionGuard = Arc<dyn Fn() -> bool + Send + Sync>;

pub(crate) async fn serve_websocket_guarded<S>(
    ws: WebSocketStream<S>,
    service: Arc<dyn RpcService>,
    guard: Option<ConnectionGuard>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let dispatch_guard = guard.clone();

    // Pump: socket <-> string channels. Ends when either side closes.
    let pump = tokio::spawn(async move {
        let mut guard_interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = guard_interval.tick(), if guard.is_some() => {
                    if !guard.as_ref().is_some_and(|guard| guard()) {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = ws_stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if guard.as_ref().is_some_and(|guard| !guard()) {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            break;
                        }
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // ping/pong/binary — ignored
                },
            }
        }
    });

    serve_connection_guarded(service, out_tx, in_rx, dispatch_guard).await;
    pump.abort();
}

#[cfg(test)]
mod authorization_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    struct RecordingService(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl RpcService for RecordingService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            self.0.lock().unwrap().push(method.to_string());
            Ok(RpcReply::Value(serde_json::Value::Bool(true)))
        }
    }

    #[tokio::test]
    async fn guarded_dispatch_rechecks_each_buffered_ndjson_call() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let checks = Arc::new(AtomicUsize::new(0));
        let guard_checks = checks.clone();
        let guard: ConnectionGuard =
            Arc::new(move || guard_checks.fetch_add(1, Ordering::SeqCst) == 0);
        let (out_tx, _out_rx) = mpsc::channel(8);
        let (in_tx, in_rx) = mpsc::channel(8);
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            serve_connection_guarded(
                Arc::new(RecordingService(server_calls)),
                out_tx,
                in_rx,
                Some(guard),
            )
            .await;
        });

        in_tx
            .send(
                "{\"id\":1,\"method\":\"BeforeRevoke\"}\n{\"id\":2,\"method\":\"AfterRevoke\"}"
                    .into(),
            )
            .await
            .unwrap();
        drop(in_tx);
        server.await.unwrap();

        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "AfterRevoke")
        );
    }

    #[tokio::test]
    async fn queued_request_rechecks_authorization_at_handle_boundary() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(AtomicBool::new(true));
        let guard_allowed = allowed.clone();
        let guard: ConnectionGuard = Arc::new(move || guard_allowed.load(Ordering::SeqCst));
        let (out_tx, _out_rx) = mpsc::channel(8);
        let service = Arc::new(RecordingService(calls.clone()));

        let queued = tokio::spawn(handle_request(
            service,
            out_tx,
            1,
            "AfterRevoke".into(),
            serde_json::Value::Null,
            Some(guard),
        ));
        allowed.store(false, Ordering::SeqCst);
        queued.await.unwrap();

        assert!(calls.lock().unwrap().is_empty());
    }
}
