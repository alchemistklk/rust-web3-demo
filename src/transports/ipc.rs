use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, atomic::AtomicUsize};
use std::task::{Context, Poll, ready};

use crate::error::{Error, Result, TransportError};
use crate::{BatchTransport, DuplexTransport, helpers};
use crate::{RequestId, Transport, api::SubscriptionId};
use futures::StreamExt;
use futures::channel::oneshot;
use futures::future::{JoinAll, join_all};
use jsonrpc_core as rpc;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
#[cfg(unix)]
use tokio_stream::wrappers::UnboundedReceiverStream;

pub struct SingleResponse(Result<oneshot::Receiver<rpc::Output>>);

impl futures::Future for SingleResponse {
    type Output = Result<rpc::Value>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.0 {
            Err(err) => Poll::Ready(Err(err.clone())),
            Ok(rx) => {
                let output = ready!(futures::Future::poll(Pin::new(rx), cx))
                    .map_err(|e| Error::Transport(TransportError::Message(e.to_string())))?;
                Poll::Ready(helpers::to_result_from_output(output))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ipc {
    id: Arc<AtomicUsize>,
    // mpsc: multi producer, single consumer
    message_tx: mpsc::UnboundedSender<TransportMessage>,
}

//
#[cfg(unix)]
impl Ipc {
    // P: AsRef<Path> represents a trait bound exists that provide flexibility with file paths in rust
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        use crate::error::{self, TransportError};

        // Connect to the Unix socket
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| error::Error::Transport(TransportError::Message(e.to_string())))?;
        Ok(Self::with_stream(stream))
    }

    fn with_stream(stream: UnixStream) -> Self {
        use tokio_stream::wrappers::UnboundedReceiverStream;

        let id = Arc::new(AtomicUsize::new(1));
        let (message_tx, _message_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_server(
            stream,
            UnboundedReceiverStream::new(_message_rx),
        ));
        Ipc { id, message_tx }
    }
}

impl Transport for Ipc {
    type Out = SingleResponse;

    fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (crate::RequestId, rpc::Call) {
        let id = self.id.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, call: rpc::Call) -> Self::Out {
        let (response_tx, response_rx) = oneshot::channel();
        let message = TransportMessage::Single((id, call, response_tx));

        SingleResponse(
            self.message_tx
                .send(message)
                .map(|()| response_rx)
                .map_err(Into::into),
        )
    }
    fn execute(&self, method: &str, params: Vec<jsonrpc_core::Value>) -> Self::Out {
        let (id, request) = self.prepare(method, params);
        self.send(id, request)
    }
}

impl BatchTransport for Ipc {
    type Batch = BatchResponse;

    fn send_batch<T: IntoIterator<Item = (RequestId, rpc::Call)>>(
        &self,
        requests: T,
    ) -> Self::Batch {
        let mut response_rxs = vec![];

        let message = TransportMessage::Batch(
            requests
                .into_iter()
                .map(|(id, call)| {
                    let (response_tx, response_rx) = oneshot::channel();
                    response_rxs.push(response_rx);

                    (id, call, response_tx)
                })
                .collect(),
        );

        BatchResponse(
            self.message_tx
                .send(message)
                .map(|()| join_all(response_rxs))
                .map_err(Into::into),
        )
    }
}

pub struct BatchResponse(Result<JoinAll<oneshot::Receiver<rpc::Output>>>);

impl futures::Future for BatchResponse {
    type Output = Result<Vec<Result<rpc::Value>>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.0 {
            Err(err) => Poll::Ready(Err(err.clone())),
            Ok(rxs) => {
                let poll = futures::Future::poll(Pin::new(rxs), cx);
                let values = ready!(poll)
                    .into_iter()
                    .map(|r| match r {
                        Ok(output) => helpers::to_result_from_output(output),
                        Err(canceled) => Err(Error::Transport(TransportError::Message(format!(
                            "Canceled: {:?}",
                            canceled
                        )))),
                    })
                    .collect();

                Poll::Ready(Ok(values))
            }
        }
    }
}

type TransportRequest = (RequestId, rpc::Call, oneshot::Sender<rpc::Output>);

#[derive(Debug)]
enum TransportMessage {
    Single(TransportRequest),
    Batch(Vec<TransportRequest>),
    Subscribe(SubscriptionId, mpsc::UnboundedSender<rpc::Value>),
    Unsubscribe(SubscriptionId),
}

/**
 * @param unix_stream: The unix domain socket connections to the Ethereum node
 * @param message_rx: Channel receiving outgoing request from the client application
 *
 */

#[cfg(unix)]
async fn run_server(
    unix_stream: UnixStream,
    messages_rx: UnboundedReceiverStream<TransportMessage>,
) -> Result<()> {
    // Split the Unix stream into reader and writer
    use tokio_util::io::ReaderStream;
    let (socket_reader, mut socket_writer) = unix_stream.into_split();

    // Maps request ids to response channel (for correlating responses)
    let mut pending_response_txs = BTreeMap::default();
    let mut subscription_txs = BTreeMap::default();

    let mut socket_reader = ReaderStream::new(socket_reader);
    // fuse
    let mut messages_rx = messages_rx.fuse();

    let mut read_buffer: Vec<u8> = vec![];
    let mut closed = false;

    while !closed || !pending_response_txs.is_empty() {
        use tokio::io::AsyncWriteExt;

        use crate::helpers;

        tokio::select! {
            message = messages_rx.next() => match message {
                None => closed = true,
                Some(TransportMessage::Subscribe(id, tx)) => {
                    if subscription_txs.insert(id.clone(), tx).is_some() {
                        log::warn!("Replacing a subscription with is {:?}", id);
                    }
                },
                Some(TransportMessage::Unsubscribe(id)) => {
                    if subscription_txs.remove(&id).is_none() {
                        log::warn!("Unsubscribe for unknown subscription: {:?}", id);
                    }
                },
                Some(TransportMessage::Single((request_id, rpc_call, response_tx))) => {
                    if pending_response_txs.insert(request_id, response_tx).is_some() {
                        log::warn!("Replacing a response with is {:?}", request_id);
                    }

                    let bytes = helpers::to_string(&rpc::Request::Single(rpc_call)).into_bytes();

                    // write the request to the socket
                    if let Err(err) = socket_writer.write_all(&bytes).await {
                        pending_response_txs.remove(&request_id);
                        log::error!("IPC write error: {:?}", err);
                    }
                },
                Some(TransportMessage::Batch(requests)) => {
                    let mut request_ids = vec![];
                    let mut rpc_calls = vec![];

                    for (request_id, rpc_call, response_tx) in requests {
                        request_ids.push(request_id);
                        rpc_calls.push(rpc_call);

                        if pending_response_txs.insert(request_id, response_tx).is_some() {
                            log::warn!("Replacing a pending request with id {:?}", request_id);
                        }
                    }
                    let bytes = helpers::to_string(&rpc::Request::Batch(rpc_calls)).into_bytes();
                    if let Err(err) = socket_writer.write_all(&bytes).await {
                        log::error!("IPC write error: {:?}", err);
                        for request_id in request_ids {
                            pending_response_txs.remove(&request_id);
                        }
                    }
                }
            },
           bytes = socket_reader.next() => match bytes {
                Some(Ok(bytes)) => {
                    // extend the read buffer with the new bytes
                    read_buffer.extend_from_slice(&bytes);

                    let read_len = {
                        // create a deserializer from the read buffer
                        let mut de: serde_json::StreamDeserializer<_, serde_json::Value> =
                            serde_json::Deserializer::from_slice(&read_buffer).into_iter();

                        while let Some(Ok(value)) = de.next() {
                            // if the value is a notification, notify the subscription
                            if let Ok(notification) = serde_json::from_value::<rpc::Notification>(value.clone()) {
                                let _ = notify(&mut subscription_txs, notification);
                                continue;
                            }

                            if let Ok(response) = serde_json::from_value::<rpc::Response>(value) {
                                let _ = respond(&mut pending_response_txs, response);
                                continue;
                            }

                            log::warn!("JSON is not a response or notification");
                        }

                        // get the byte offset of the last valid JSON object
                        de.byte_offset()
                    };

                    read_buffer.copy_within(read_len.., 0);
                    read_buffer.truncate(read_buffer.len() - read_len);
                },
                Some(Err(err)) => {
                    log::error!("IPC read error: {:?}", err);
                    return Err(Error::Transport(TransportError::Message(err.to_string())));
                },
                None => break,
            }
        };
    }
    Ok(())
}

fn notify(
    subscription_txs: &mut BTreeMap<SubscriptionId, mpsc::UnboundedSender<rpc::Value>>,
    notification: rpc::Notification,
) -> std::result::Result<(), ()> {
    if let rpc::Params::Map(params) = notification.params {
        let id = params.get("subscription");
        let result = params.get("result");

        if let (Some(&rpc::Value::String(ref id)), Some(result)) = (id, result) {
            let id = id.clone().into();
            if let Some(tx) = subscription_txs.get(&id) {
                // send the result to the subscription
                if let Err(err) = tx.send(result.clone()) {
                    log::error!("Failed to send notification to subscription: {:?}", err);
                }
            } else {
                log::warn!("Subscription not found: {:?}", id);
            }
        } else {
            log::error!("Notification params are not a map");
        }
    }
    Ok(())
}

fn respond(
    pending_response_txs: &mut BTreeMap<RequestId, oneshot::Sender<rpc::Output>>,
    response: rpc::Response,
) -> std::result::Result<(), ()> {
    // dispatch the response to the client
    let outputs = match response {
        rpc::Response::Single(output) => vec![output],
        rpc::Response::Batch(outputs) => outputs,
    };
    for output in outputs {
        let _ = respond_output(pending_response_txs, output);
    }
    Ok(())
}

fn respond_output(
    pending_response_txs: &mut BTreeMap<RequestId, oneshot::Sender<rpc::Output>>,
    output: rpc::Output,
) -> std::result::Result<(), ()> {
    let id = output.id().clone();

    // convert the id to a usize
    let id = match id {
        rpc::Id::Num(num) => num as usize,
        _ => {
            log::warn!("Got unsupported response (id: {:?})", id);
            return Err(());
        }
    };

    // remove the response tx from the map
    let response_tx = pending_response_txs.remove(&id).ok_or_else(|| {
        log::warn!("No response found for id: {:?}", id);
    })?;

    // send the response to the client
    let _ = response_tx.send(output).map_err(|e| {
        log::error!("Failed to send response to client: {:?}", e);
    });

    Ok(())
}

impl From<mpsc::error::SendError<TransportMessage>> for Error {
    fn from(err: mpsc::error::SendError<TransportMessage>) -> Self {
        Error::Transport(TransportError::Message(format!("Send Error: {:?}", err)))
    }
}

impl DuplexTransport for Ipc {
    type NotificationStream = UnboundedReceiverStream<rpc::Value>;

    fn subscribe(&self, id: SubscriptionId) -> Result<Self::NotificationStream> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.message_tx.send(TransportMessage::Subscribe(id, tx))?;
        Ok(UnboundedReceiverStream::new(rx))
    }

    fn unsubscribe(&self, id: SubscriptionId) -> Result<()> {
        self.message_tx
            .send(TransportMessage::Unsubscribe(id))
            .map_err(Into::into)
    }
}

#[cfg(all(test, unix))]
mod test {
    use super::*;
    use futures::future::join_all;
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::UnixStream};
    use tokio_util::io::ReaderStream;

    #[tokio::test]
    async fn works_for_single_requests() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_single(stream2));

        let (req_id, request) = ipc.prepare(
            "eth_test",
            vec![json!({
                "test": -1,
            })],
        );
        let response = ipc.send(req_id, request).await;
        let expected_response_json: serde_json::Value = json!({
            "test": 1,
        });
        assert_eq!(response, Ok(expected_response_json));

        let (req_id, request) = ipc.prepare(
            "eth_test",
            vec![json!({
                "test": 3,
            })],
        );
        let response = ipc.send(req_id, request).await;
        let expected_response_json: serde_json::Value = json!({
            "test": "string1",
        });
        assert_eq!(response, Ok(expected_response_json));
    }

    async fn eth_node_single(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();

        let mut rx = ReaderStream::new(rx);
        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!({
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 1,
                    "params": [{
                        "test": -1
                    }]
                })
            );

            tx.write_all(r#"{"jsonrpc": "2.0", "id": 1, "result": {"test": 1}}"#.as_ref())
                .await
                .unwrap();
            tx.flush().await.unwrap();
        }

        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!({
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 2,
                    "params": [{
                        "test": 3
                    }]
                })
            );

            let response_bytes = r#"{"jsonrpc": "2.0", "id": 2, "result": {"test": "string1"}}"#;
            for chunk in response_bytes.as_bytes().chunks(3) {
                tx.write_all(chunk).await.unwrap();
                tx.flush().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn works_for_batch_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_batch(stream2));

        let requests = vec![json!({"test": -1,}), json!({"test": 3,})];
        let requests = requests
            .into_iter()
            .map(|v| ipc.prepare("eth_test", vec![v]));

        let response = ipc.send_batch(requests).await;
        let expected_response_json = vec![Ok(json!({"test": 1})), Ok(json!({"test": "string1"}))];

        assert_eq!(response, Ok(expected_response_json));
    }

    async fn eth_node_batch(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();

        let mut rx = ReaderStream::new(rx);
        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!([{
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 1,
                    "params": [{
                        "test": -1
                    }]
                }, {
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 2,
                    "params": [{
                        "test": 3
                    }]
                }])
            );

            let response = json!([
                {"jsonrpc": "2.0", "id": 1, "result": {"test": 1}},
                {"jsonrpc": "2.0", "id": 2, "result": {"test": "string1"}},
            ]);

            tx.write_all(serde_json::to_string(&response).unwrap().as_ref())
                .await
                .unwrap();

            tx.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn works_for_partial_batches() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_partial_batches(stream2));

        let requests = vec![json!({"test": 0}), json!({"test": 1}), json!({"test": 2})];
        let requests = requests
            .into_iter()
            .map(|v| ipc.execute("eth_test", vec![v]));
        let responses = join_all(requests).await;

        assert_eq!(responses[0], Ok(json!({"test": 0})));
        assert_eq!(responses[2], Ok(json!({"test": 2})));
        assert!(responses[1].is_err());
    }

    async fn eth_node_partial_batches(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();
        let mut buf = vec![];
        let mut rx = ReaderStream::new(rx);
        while let Some(Ok(bytes)) = rx.next().await {
            buf.extend(bytes);

            let requests: std::result::Result<Vec<serde_json::Value>, serde_json::Error> =
                serde_json::Deserializer::from_slice(&buf)
                    .into_iter()
                    .collect();

            if let Ok(requests) = requests {
                if requests.len() == 3 {
                    break;
                }
            }
        }

        let response = json!([
            {"jsonrpc": "2.0", "id": 1, "result": {"test": 0}},
            {"jsonrpc": "2.0", "id": "2", "result": {"test": 2}},
            {"jsonrpc": "2.0", "id": 3, "result": {"test": 2}},
        ]);

        tx.write_all(serde_json::to_string(&response).unwrap().as_ref())
            .await
            .unwrap();

        tx.flush().await.unwrap();
    }
}
