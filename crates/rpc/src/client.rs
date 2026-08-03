//! Client side: request/stream multiplexing over string frames + the WebSocket dialer.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{ClientFrame, RpcError, ServerFrame};

enum Pending {
    Call(oneshot::Sender<Result<serde_json::Value, RpcError>>),
    Stream(mpsc::UnboundedSender<serde_json::Value>),
}

struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
}

struct WriteRequest {
    frame: String,
    delivered: oneshot::Sender<Result<(), RpcError>>,
}

struct PendingGuard {
    id: u64,
    shared: Arc<Shared>,
    cancels: mpsc::Sender<String>,
    writer: tokio::task::AbortHandle,
    delivered: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let removed = self.shared.lock().remove(&self.id).is_some();
        if !removed {
            return;
        }
        if !self.delivered {
            self.writer.abort();
            return;
        }
        if let Ok(frame) = cancel_frame(self.id)
            && matches!(
                self.cancels.try_send(frame),
                Err(mpsc::error::TrySendError::Full(_))
            )
        {
            self.writer.abort();
        }
    }
}

async fn run_writer(
    transport: mpsc::Sender<String>,
    mut requests: mpsc::Receiver<WriteRequest>,
    mut cancels: mpsc::Receiver<String>,
) {
    loop {
        tokio::select! {
            biased;
            cancel = cancels.recv() => match cancel {
                Some(frame) => {
                    if transport.try_send(frame).is_err() { return; }
                    continue;
                }
                None => return,
            },
            request = requests.recv() => {
                let Some(request) = request else { return; };
                let WriteRequest { frame, delivered } = request;
                let request_transport = transport.clone();
                let send = request_transport.send(frame);
                tokio::pin!(send);
                loop {
                    tokio::select! {
                        biased;
                        cancel = cancels.recv() => match cancel {
                            Some(frame) if transport.try_send(frame.clone()).is_ok() => continue,
                            Some(_) | None => {
                                let _ = delivered.send(Err(RpcError::Closed));
                                return;
                            }
                        },
                        result = &mut send => {
                            let result = result.map_err(|_| RpcError::Closed);
                            let failed = result.is_err();
                            let _ = delivered.send(result);
                            if failed { return; }
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub struct RpcStream {
    inbound: mpsc::UnboundedReceiver<serde_json::Value>,
    _guard: PendingGuard,
}

impl RpcStream {
    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        self.inbound.recv().await
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A multiplexing RPC client over any string-frame duplex ([`crate::memory_client`] or
/// [`connect_ws`]). Cheap to clone-by-Arc internally; use one per connection.
pub struct RpcClient {
    requests: mpsc::Sender<WriteRequest>,
    cancels: mpsc::Sender<String>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl RpcClient {
    /// Wrap an existing duplex: `out` carries client frames, `inbound` server frames.
    pub fn new(out: mpsc::Sender<String>, mut inbound: mpsc::Receiver<String>) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
        });
        let (requests, request_rx) = mpsc::channel(256);
        let (cancels, cancel_rx) = mpsc::channel(64);
        let writer = tokio::spawn(run_writer(out, request_rx, cancel_rx));
        let reader_shared = shared.clone();
        let reader = tokio::spawn(async move {
            while let Some(payload) = inbound.recv().await {
                for line in payload.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame: ServerFrame = match serde_json::from_str(line) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::warn!(error = %err, "rpc: dropping malformed server frame");
                            continue;
                        }
                    };
                    route_frame(&reader_shared, frame);
                }
            }
            // Connection closed: fail everything still pending.
            let drained: Vec<Pending> = {
                let mut pending = reader_shared.lock();
                pending.drain().map(|(_, p)| p).collect()
            };
            for entry in drained {
                if let Pending::Call(tx) = entry {
                    let _ = tx.send(Err(RpcError::Closed));
                }
                // Streams end by sender drop.
            }
        });
        Self {
            requests,
            cancels,
            shared,
            next_id: AtomicU64::new(1),
            reader,
            writer,
        }
    }

    /// Unary request.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let (tx, rx) = oneshot::channel();
        let id = self.register(Pending::Call(tx));
        let mut guard = PendingGuard {
            id,
            shared: self.shared.clone(),
            cancels: self.cancels.clone(),
            writer: self.writer.abort_handle(),
            delivered: false,
        };
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        guard.delivered = true;
        rx.await.map_err(|_| RpcError::Closed)?
    }

    /// Typed unary request.
    pub async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| RpcError::BadParams(e.to_string()))
    }

    /// Streaming request: items arrive on the stream; it closes when the server sends
    /// `{done}` or `{err}`, or the connection drops. Dropping the stream immediately
    /// sends `{id, cancel}` and removes its pending entry.
    pub async fn subscribe(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcStream, RpcError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.register(Pending::Stream(tx));
        let guard = PendingGuard {
            id,
            shared: self.shared.clone(),
            cancels: self.cancels.clone(),
            writer: self.writer.abort_handle(),
            delivered: false,
        };
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        let mut guard = guard;
        guard.delivered = true;
        Ok(RpcStream {
            inbound: rx,
            _guard: guard,
        })
    }

    fn register(&self, pending: Pending) -> u64 {
        let mut pending = Some(pending);
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let mut entries = self.shared.lock();
            if let Entry::Vacant(slot) = entries.entry(id) {
                slot.insert(pending.take().expect("pending request already registered"));
                return id;
            }
            // Only reachable after u64 wraparound while the old id is still live.
        }
    }

    async fn send(&self, frame: ClientFrame) -> Result<(), RpcError> {
        let json = serde_json::to_string(&frame)
            .map_err(|e| RpcError::Transport(format!("serialize frame: {e}")))?;
        let (delivered, received) = oneshot::channel();
        self.requests
            .send(WriteRequest {
                frame: json,
                delivered,
            })
            .await
            .map_err(|_| RpcError::Closed)?;
        received.await.map_err(|_| RpcError::Closed)?
    }
}

fn cancel_frame(id: u64) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ClientFrame {
        id,
        method: None,
        params: serde_json::Value::Null,
        cancel: true,
    })
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

fn route_frame(shared: &Arc<Shared>, frame: ServerFrame) {
    let id = frame.id;
    if let Some(err) = frame.err {
        match shared.lock().remove(&id) {
            Some(Pending::Call(tx)) => {
                let _ = tx.send(Err(RpcError::Failed(err)));
            }
            Some(Pending::Stream(_)) | None => {
                // Stream errored: the sender drop closes the receiver.
                tracing::debug!(id, %err, "rpc: stream ended with error");
            }
        }
        return;
    }
    if let Some(value) = frame.ok {
        if let Some(Pending::Call(tx)) = shared.lock().remove(&id) {
            let _ = tx.send(Ok(value));
        }
        return;
    }
    if let Some(item) = frame.item {
        if let Some(Pending::Stream(tx)) = shared.lock().get(&id) {
            let _ = tx.send(item);
        }
        return;
    }
    if frame.done {
        shared.lock().remove(&id);
    }
}

/// How long a dial may take before we give up.
///
/// This is localhost: a real engine answers in milliseconds. Without a bound,
/// *any* other process holding the port accepts the TCP connection and then
/// never completes the WebSocket handshake, and the caller waits forever — a
/// stranger on port 27654 would hang the app at boot rather than degrade it.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Dial a WebSocket RPC server (`ws://127.0.0.1:{ipc_port}`).
pub async fn connect_ws(url: &str) -> Result<RpcClient, RpcError> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| RpcError::Transport(format!("timed out dialing {url}")))?
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        loop {
            tokio::select! {
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
                message = stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    });
    Ok(RpcClient::new(out_tx, in_rx))
}

#[cfg(test)]
mod cancellation_backpressure_tests {
    use std::future::Future;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use futures::task::noop_waker;

    use super::*;

    #[tokio::test]
    async fn dropping_many_calls_on_a_full_transport_is_prompt_and_closes_it() {
        let (out, mut outbound) = mpsc::channel(1);
        out.try_send("occupied".into()).unwrap();
        let (inbound_sender, inbound) = mpsc::channel(1);
        let client = RpcClient::new(out, inbound);
        let mut calls: Vec<_> = (0..32)
            .map(|_| Box::pin(client.call("Never", serde_json::Value::Null)))
            .collect();
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        for call in &mut calls {
            assert!(matches!(call.as_mut().poll(&mut context), Poll::Pending));
        }
        let streams: Vec<_> = (0..32)
            .map(|_| {
                let (sender, inbound) = mpsc::unbounded_channel();
                let id = client.register(Pending::Stream(sender));
                RpcStream {
                    inbound,
                    _guard: PendingGuard {
                        id,
                        shared: client.shared.clone(),
                        cancels: client.cancels.clone(),
                        writer: client.writer.abort_handle(),
                        delivered: true,
                    },
                }
            })
            .collect();
        assert_eq!(client.shared.lock().len(), calls.len() + streams.len());

        let started = Instant::now();
        drop(calls);
        drop(streams);
        assert!(started.elapsed() < Duration::from_millis(20));
        assert!(client.shared.lock().is_empty());
        assert_eq!(outbound.recv().await.as_deref(), Some("occupied"));
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), outbound.recv())
                .await
                .expect("full cancel queue did not close the transport"),
            None
        );
        drop(inbound_sender);
    }

    #[test]
    fn dropping_with_a_full_transport_outside_a_runtime_is_prompt() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let writer = runtime.spawn(futures::future::pending::<()>());
        let (cancels, _cancel_rx) = mpsc::channel(1);
        cancels.try_send("occupied".into()).unwrap();
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::from([(
                1,
                Pending::Stream(mpsc::unbounded_channel().0),
            )])),
        });
        let guard = PendingGuard {
            id: 1,
            shared,
            cancels,
            writer: writer.abort_handle(),
            delivered: true,
        };
        let (finished, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            drop(guard);
            let _ = finished.send(());
        });

        let prompt = received.recv_timeout(Duration::from_millis(50)).is_ok();
        thread.join().unwrap();
        assert!(prompt, "PendingGuard::drop blocked outside a runtime");
    }
}
