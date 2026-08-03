use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use comet_rpc::{RpcError, RpcReply, RpcService};
use futures::StreamExt;

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct CancellationService {
    unary_started: Arc<tokio::sync::Notify>,
    unary_cancelled: Arc<AtomicBool>,
    stream_started: Arc<tokio::sync::Notify>,
    stream_cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl RpcService for CancellationService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            "UnaryNever" => {
                let _dropped = Dropped(self.unary_cancelled.clone());
                self.unary_started.notify_one();
                futures::future::pending().await
            }
            "StreamNever" => {
                self.stream_started.notify_one();
                let dropped = Dropped(self.stream_cancelled.clone());
                Ok(RpcReply::Stream(
                    futures::stream::unfold(dropped, |dropped| async move {
                        let _dropped = dropped;
                        futures::future::pending().await
                    })
                    .boxed(),
                ))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

fn fixture() -> (
    comet_rpc::RpcClient,
    Arc<tokio::sync::Notify>,
    Arc<AtomicBool>,
    Arc<tokio::sync::Notify>,
    Arc<AtomicBool>,
) {
    let unary_started = Arc::new(tokio::sync::Notify::new());
    let unary_cancelled = Arc::new(AtomicBool::new(false));
    let stream_started = Arc::new(tokio::sync::Notify::new());
    let stream_cancelled = Arc::new(AtomicBool::new(false));
    let client = comet_rpc::memory_client(Arc::new(CancellationService {
        unary_started: unary_started.clone(),
        unary_cancelled: unary_cancelled.clone(),
        stream_started: stream_started.clone(),
        stream_cancelled: stream_cancelled.clone(),
    }));
    (
        client,
        unary_started,
        unary_cancelled,
        stream_started,
        stream_cancelled,
    )
}

async fn wait_cancelled(cancelled: &AtomicBool) {
    tokio::time::timeout(Duration::from_millis(250), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server-side RPC work was not cancelled");
}

#[tokio::test]
async fn dropping_unary_future_cancels_server_work_without_a_response() {
    let (client, started, cancelled, _, _) = fixture();
    let mut call = Box::pin(client.call("UnaryNever", serde_json::Value::Null));
    tokio::select! {
        result = &mut call => panic!("unary unexpectedly completed: {result:?}"),
        () = started.notified() => {}
    }

    drop(call);

    wait_cancelled(&cancelled).await;
}

#[tokio::test]
async fn dropping_stream_owner_cancels_server_work_without_another_item() {
    let (client, _, _, started, cancelled) = fixture();
    let stream = client
        .subscribe("StreamNever", serde_json::Value::Null)
        .await
        .unwrap();
    started.notified().await;

    drop(stream);

    wait_cancelled(&cancelled).await;
}
