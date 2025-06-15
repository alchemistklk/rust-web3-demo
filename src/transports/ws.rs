use core::fmt;
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, atomic},
    task::{Context, Poll, ready},
};

use self::compat::{TcpStream, TlsStream};
use crate::{
    BatchTransport, DuplexTransport, RequestId, Transport,
    api::SubscriptionId,
    error::{self, Error, TransportError},
    helpers, rpc,
};
use futures::{
    AsyncRead, AsyncWrite, FutureExt, Stream, StreamExt,
    channel::{mpsc, oneshot},
    pin_mut, select,
};
use soketto::{
    connection,
    handshake::{Client, ServerResponse},
};
use url::Url;

// single result
type SingleResult = error::Result<rpc::Value>;

// batch result
type BatchResult = error::Result<Vec<SingleResult>>;

// single-use channel sender
type Pending = oneshot::Sender<BatchResult>;

// unbounded multi-producer, single-consumer channel
type Subscription = mpsc::UnboundedSender<rpc::Value>;

// Stream, either plain tcp or tls
enum MaybeTlsStream<P, T> {
    // Plain Stream
    Plain(P),
    // TLS Stream
    Tls(T),
}

impl<P, T> AsyncRead for MaybeTlsStream<P, T>
where
    P: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        // Pin ensures no movement
        self: std::pin::Pin<&mut Self>,
        // context for the task
        cx: &mut std::task::Context<'_>,
        // buffer to read into
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl<P, T> AsyncWrite for MaybeTlsStream<P, T>
where
    P: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        // Pin ensures no movement
        self: std::pin::Pin<&mut Self>,
        // context for the task
        cx: &mut std::task::Context<'_>,
        // buffer to write from
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_close(cx),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_close(cx),
        }
    }
}

struct WsServerTask {
    // Tracks outstanding requests waiting for responses
    pending: BTreeMap<RequestId, Pending>,
    // WebSocket message sender that wraps a stream of type `T`
    sender: connection::Sender<MaybeTlsStream<TcpStream, TlsStream>>,
    // WebSocket message receiver that wraps a stream of type `T`
    receiver: connection::Receiver<MaybeTlsStream<TcpStream, TlsStream>>,
    // Subscription management
    subscriptions: BTreeMap<SubscriptionId, Subscription>,
}

impl WsServerTask {
    // Create a new WsServerTask transport
    pub async fn new(url: &str) -> error::Result<Self> {
        let url = Url::parse(url).map_err(|e| {
            error::Error::Transport(TransportError::Message(format!("Invalid URL: {}", e)))
        })?;

        let schema = match url.scheme() {
            s if s == "ws" || s == "wss" => s,
            s => {
                return Err(error::Error::Transport(TransportError::Message(format!(
                    "Wrong scheme: {}",
                    s
                ))));
            }
        };

        let host = match url.host_str() {
            Some(h) => h,
            None => {
                return Err(error::Error::Transport(TransportError::Message(
                    "Missing host in URL".to_string(),
                )));
            }
        };

        let port = url
            .port()
            .unwrap_or_else(|| if schema == "ws" { 80 } else { 443 });
        let addr = format!("{}:{}", host, port);

        log::trace!("Connecting to TcpStream at {}", addr);

        // Create a raw tokio TcpStream
        let stream = compat::raw_tcp_stream(addr).await.unwrap();
        // disable nagle algorithm for low latency
        stream.set_nodelay(true).map_err(|e| {
            error::Error::Transport(TransportError::Message(format!(
                "Failed to set nodelay: {}",
                e
            )))
        })?;

        // Wrap the stream in a MaybeTlsStream based on the URL scheme
        let socket = if schema == "wss" {
            #[cfg(any(feature = "ws-tls-tokio", feature = "ws-tls-tokio"))]
            {
                // wrap existing stream with tls encryption
                let tls_stream = async_native_tls::TlsConnector::new()
                    .connect(host, stream)
                    .await
                    .map_err(|e| {
                        error::Error::Transport(TransportError::Message(format!(
                            "TLS connection failed: {}",
                            e
                        )))
                    })?;
                let stream = compat::compat(tls_stream);
                MaybeTlsStream::Tls(stream)
            }
        } else {
            let stream = compat::compat(stream);
            MaybeTlsStream::Plain(stream)
        };

        // Confirm the resource that we are connecting to
        let resource = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };

        log::trace!(
            "Connecting websocket client with host: {} and resource: {}",
            host,
            resource
        );

        let mut client = Client::new(socket, host, &resource);

        // todo: need to figure out how to encode and decode headers
        let maybe_encoded = url.password().map(|password| {
            use headers::authorization::{Authorization, Credentials};
            Authorization::basic(url.username(), password)
                .0
                .encode()
                .as_bytes()
                .to_vec()
        });

        let headers = maybe_encoded.as_ref().map(|head| {
            [soketto::handshake::client::Header {
                name: "Authorization",
                value: head,
            }]
        });

        if let Some(ref head) = headers {
            client.set_headers(head);
        }

        let handshake = client.handshake();

        // create a sender and receiver
        let (sender, receiver) = match handshake.await.map_err(|e| {
            error::Error::Transport(TransportError::Message(format!("Handshake failed: {}", e)))
        })? {
            ServerResponse::Accepted { .. } => client.into_builder().finish(),
            ServerResponse::Redirect { status_code, .. } => {
                return Err(error::Error::Transport(TransportError::Code(status_code)));
            }
            ServerResponse::Rejected { status_code } => {
                return Err(error::Error::Transport(TransportError::Code(status_code)));
            }
        };

        Ok(Self {
            pending: Default::default(),
            sender,
            receiver,
            subscriptions: Default::default(),
        })
    }

    async fn into_task(self, requests: mpsc::UnboundedReceiver<TransportMessage>) {
        let Self {
            receiver,
            mut sender,
            mut pending,
            mut subscriptions,
        } = self;

        let receiver = as_data_stream(receiver).fuse();
        let requests = requests.fuse();
        pin_mut!(receiver);
        pin_mut!(requests);

        loop {
            select! {
                msg = requests.next() => match msg {
                    Some(TransportMessage::Request {
                        id,
                        request,
                        sender: tx
                    }) => {
                        // store the response channel for the pending map
                        if pending.insert(id, tx).is_some() {
                            log::warn!("Request with id {} already exists", id);
                        }

                        // send the request over websocket
                        let res = sender.send_text(request).await;
                        let res2 = sender.flush().await;
                        // handle send errors
                        if let Err(e) = res.and(res2) {
                            log::error!("Failed to send request: {}", e);
                            pending.remove(&id);
                        }
                    }
                    // Store the subscription channel in the subscriptions map
                    Some(TransportMessage::Subscribe { id, sink }) => {
                        if subscriptions.insert(id.clone(), sink).is_some() {
                            log::warn!("Subscription with id {:?} already exists", id);
                        }
                    }

                    // Clean up the subscription channel from the subscriptions map
                    Some(TransportMessage::Unsubscribe { id }) => {
                        subscriptions.remove(&id);
                    }
                    None => break,
                },
                // Handle incoming messages from the websocket
                res = receiver.next() => match res {
                    Some(Ok(data)) => {
                        handle_message(&data, &subscriptions, &mut pending);
                    },
                    Some(Err(e)) => {
                        log::error!("Failed to receive message: {}", e);
                        break;
                    }
                    None => break,
                },
                complete => break

            }
        }
    }
}

fn handle_message(
    data: &[u8],
    subscriptions: &BTreeMap<SubscriptionId, Subscription>,
    pending: &mut BTreeMap<RequestId, Pending>,
) {
    log::trace!("Message received {:?}", data);
    // covert data slice into a notification
    if let Ok(notification) = helpers::to_notification_from_slice(data) {
        if let rpc::Params::Map(params) = notification.params {
            let id = params.get("subscription");
            let result = params.get("result");
            if let (Some(&rpc::Value::String(ref id)), Some(result)) = (id, result) {
                let id: SubscriptionId = id.clone().into();
                if let Some(stream) = subscriptions.get(&id) {
                    if let Err(e) = stream.unbounded_send(result.clone()) {
                        log::error!("Error sending notification: {:?} (id: {:?}", e, id);
                    }
                } else {
                    log::warn!("Got notification for unknown subscription (id: {:?})", id);
                }
            } else {
                log::error!("Got unsupported notification (id: {:?})", id);
            }
        }
    } else {
        // convert data slice into a response
        let response = helpers::to_response_from_slice(data);
        // handle the output
        let output = match response {
            Ok(rpc::Response::Single(output)) => vec![output],
            Ok(rpc::Response::Batch(outputs)) => outputs,
            _ => vec![],
        };
        // handle the id
        let id = match output.get(0) {
            Some(&rpc::Output::Success(ref success)) => success.id.clone(),
            Some(&rpc::Output::Failure(ref failure)) => failure.id.clone(),
            None => rpc::Id::Num(0),
        };

        if let rpc::Id::Num(num) = id {
            if let Some(request) = pending.remove(&(num as usize)) {
                log::trace!("Responding to (id: {:?}) with {:?}", num, output);
                // Error explanation: The type mismatch occurs because `helpers::to_results_from_outputs()` returns
                // `Result<Vec<Result<Value, error::Error>>, _>` but the channel expects `Result<Vec<Value>, _>`.
                // We need to resolve the inner Results before sending.
                if let Err(err) = request.send(helpers::to_results_from_outputs(output)) {
                    log::warn!("Sending a response to deallocated channel: {:?}", err);
                }
            } else {
                log::warn!("Got response for unknown request (id: {:?})", num);
            }
        } else {
            log::warn!("Got unsupported response (id: {:?})", id);
        }
    }
}

// transform the soketto receiver into a stream of data
fn as_data_stream<T: Unpin + futures::AsyncRead + futures::AsyncWrite>(
    receiver: connection::Receiver<T>,
) -> impl Stream<Item = Result<Vec<u8>, soketto::connection::Error>> {
    futures::stream::unfold(receiver, |mut receiver| async move {
        let mut data = Vec::new();
        Some(match receiver.receive_data(&mut data).await {
            Ok(_) => (Ok(data), receiver),
            Err(e) => (Err(e), receiver),
        })
    })
}

#[cfg(feature = "ws-tokio")]
pub mod compat {
    use std::io;
    use tokio::io::AsyncRead;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

    // Make Tokio's TcpStream compatible with future
    pub type TcpStream = Compat<tokio::net::TcpStream>;

    // TcpListener accepting connections, no wrapper needed
    pub type TcpListener = tokio::net::TcpListener;

    // TLS stream using async-native-tls with Tokio's TcpStream, Compatible with AsyncRead/Write
    #[cfg(feature = "ws-tls-tokio")]
    pub type TlsStream = Compat<async_native_tls::TlsStream<tokio::net::TcpStream>>;

    pub async fn raw_tcp_stream(addrs: String) -> io::Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addrs).await
    }

    // Wrap given argument into compatibility layer.
    pub fn compat<T: AsyncRead>(t: T) -> Compat<T> {
        TokioAsyncReadCompatExt::compat(t)
    }
}

// definition of transport messages
enum TransportMessage {
    Request {
        id: RequestId,
        request: String,
        sender: oneshot::Sender<BatchResult>,
    },
    Subscribe {
        id: SubscriptionId,
        sink: mpsc::UnboundedSender<rpc::Value>,
    },
    Unsubscribe {
        id: SubscriptionId,
    },
}

// WebSocket transport
#[derive(Clone)]
pub struct WebSocket {
    id: Arc<atomic::AtomicUsize>,
    requests: mpsc::UnboundedSender<TransportMessage>,
}

// This implements the `Debug` which controls how it appear when printed with `{:?}` format
impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocket").field("id", &self.id).finish()
    }
}

impl WebSocket {
    pub async fn new(url: &str) -> error::Result<Self> {
        let id = Arc::new(atomic::AtomicUsize::new(1));
        let task = WsServerTask::new(url).await?;
        // unbounded sender: never blocks when sending
        // unbounded receiver: asynchronously without blocking the sender
        let (sink, stream) = mpsc::unbounded();
        #[cfg(feature = "ws-tokio")]
        tokio::spawn(task.into_task(stream));
        #[cfg(feature = "ws-async-std")]
        async_std::task::spawn(task.into_task(stream));
        Ok(Self { id, requests: sink })
    }

    fn send(&self, msg: TransportMessage) {
        if let Err(e) = self.requests.unbounded_send(msg) {
            log::error!("Failed to send message: {:?}", e);
        }
    }

    // Oneshot channel provide a simple, efficient, way to send a single value from one async task to another, then automatically clean up
    // After sending/receiving the value, the channel is automatically dropped
    fn send_request(
        &self,
        id: RequestId,
        request: rpc::Request,
    ) -> error::Result<oneshot::Receiver<BatchResult>> {
        let request = helpers::to_string(&request);
        log::debug!("[{}] Calling: {}", id, request);

        let (sender, receiver) = oneshot::channel();
        self.send(TransportMessage::Request {
            id,
            request,
            sender,
        });
        Ok(receiver)
    }
}

fn dropped_err<T>(_: T) -> error::Error {
    Error::Transport(TransportError::Message(
        "Cannot send request. Internal task finished.".into(),
    ))
}

fn batch_to_single(response: BatchResult) -> SingleResult {
    match response?.into_iter().next() {
        Some(res) => res,
        None => Err(Error::InvalidResponse("Expected single, got batch.".into())),
    }
}

fn batch_to_batch(res: BatchResult) -> BatchResult {
    res
}

enum ResponseState {
    Receiver(Option<error::Result<oneshot::Receiver<BatchResult>>>),
    Waiting(oneshot::Receiver<BatchResult>),
}

pub struct Response<R, T> {
    // extract data
    extract: T,
    state: ResponseState,
    // phantom data means it doesn't directly store the data, but it maintains the relationship,(raw data)
    _data: std::marker::PhantomData<R>,
}

impl<R, T> Response<R, T> {
    fn new(response: error::Result<oneshot::Receiver<BatchResult>>, extract: T) -> Self {
        Self {
            extract,
            state: ResponseState::Receiver(Some(response)),
            _data: std::marker::PhantomData,
        }
    }
}

impl<R, T> Future for Response<R, T>
where
    R: Unpin + 'static,
    // transform the `batch result` into single result
    T: Fn(BatchResult) -> error::Result<R> + Unpin + 'static,
{
    type Output = error::Result<R>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.state {
                ResponseState::Receiver(ref mut res) => {
                    let receiver = res
                        .take()
                        .expect("Receiver state is active only once; qed")?;
                    self.state = ResponseState::Waiting(receiver)
                }
                ResponseState::Waiting(ref mut future) => {
                    let response = ready!(future.poll_unpin(cx)).map_err(dropped_err)?;
                    return Poll::Ready((self.extract)(response));
                }
            }
        }
    }
}

// This implementation provides a standardized interface for making a single `JSON-RPC` call.
// The actual response contains a fn trait that can be used to transform which provides the flexibility to handle both single and batch responses.
impl Transport for WebSocket {
    type Out = Response<rpc::Value, fn(BatchResult) -> SingleResult>;

    fn prepare(
        &self,
        method: &str,
        params: Vec<jsonrpc_core::Value>,
    ) -> (RequestId, jsonrpc_core::Call) {
        let id = self.id.fetch_add(1, atomic::Ordering::AcqRel);
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, request: jsonrpc_core::Call) -> Self::Out {
        let response = self.send_request(id, rpc::Request::Single(request));
        Response::new(response, batch_to_single)
    }
}

// This implementation provides a standardized interface for making a batch `JSON-RPC` call.
impl BatchTransport for WebSocket {
    type Batch = Response<Vec<SingleResult>, fn(BatchResult) -> BatchResult>;

    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, rpc::Call)>,
    {
        let mut it = requests.into_iter();
        // pick out the id that represents the whole batch request
        let (id, first) = it
            .next()
            .map(|x| (x.0, Some(x.1)))
            .unwrap_or_else(|| (0, None));
        // collect the rest of the requests into a vector
        let requests = first.into_iter().chain(it.map(|x| x.1)).collect();
        let response = self.send_request(id, rpc::Request::Batch(requests));
        Response::new(response, batch_to_batch)
    }
}

impl DuplexTransport for WebSocket {
    type NotificationStream = mpsc::UnboundedReceiver<rpc::Value>;

    fn subscribe(&self, id: SubscriptionId) -> error::Result<Self::NotificationStream> {
        // TODO [ToDr] Not unbounded?
        let (sink, stream) = mpsc::unbounded();
        self.send(TransportMessage::Subscribe { id, sink });
        Ok(stream)
    }

    fn unsubscribe(&self, id: SubscriptionId) -> error::Result<()> {
        self.send(TransportMessage::Unsubscribe { id });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rpc, Transport};
    use futures::{
        io::{BufReader, BufWriter},
        StreamExt,
    };
    use soketto::handshake;
    use tokio_stream::wrappers::TcpListenerStream;

    #[test]
    fn bounds_matching() {
        fn async_rw<T: AsyncRead + AsyncWrite>() {}

        async_rw::<TcpStream>();
        async_rw::<MaybeTlsStream<TcpStream, TlsStream>>();
    }

    #[tokio::test]
    async fn should_send_a_request() {
        let _ = env_logger::try_init();
        // given
        let addr = "127.0.0.1:3000";
        let listener = futures::executor::block_on(compat::TcpListener::bind(addr)).expect("Failed to bind");
        println!("Starting the server.");
        tokio::spawn(server(listener, addr));

        let endpoint = "ws://127.0.0.1:3000";
        let ws = WebSocket::new(endpoint).await.unwrap();

        // when
        let res = ws.execute("eth_accounts", vec![rpc::Value::String("1".into())]);

        // then
        assert_eq!(res.await.unwrap(), rpc::Value::String("x".into()));
    }

    async fn server(listener: compat::TcpListener, addr: &str) {
        let mut incoming = TcpListenerStream::new(listener);
        println!("Listening on: {}", addr);
        while let Some(Ok(socket)) = incoming.next().await {
            let socket = compat::compat(socket);
            let mut server = handshake::Server::new(BufReader::new(BufWriter::new(socket)));
            let key = {
                let req = server.receive_request().await.unwrap();
                req.key()
            };
            let accept = handshake::server::Response::Accept { key, protocol: None };
            server.send_response(&accept).await.unwrap();
            let (mut sender, mut receiver) = server.into_builder().finish();
            loop {
                let mut data = Vec::new();
                match receiver.receive_data(&mut data).await {
                    Ok(data_type) if data_type.is_text() => {
                        assert_eq!(
                            std::str::from_utf8(&data),
                            Ok(r#"{"jsonrpc":"2.0","method":"eth_accounts","params":["1"],"id":1}"#)
                        );
                        sender
                            .send_text(r#"{"jsonrpc":"2.0","id":1,"result":"x"}"#)
                            .await
                            .unwrap();
                        sender.flush().await.unwrap();
                    }
                    Err(soketto::connection::Error::Closed) => break,
                    e => panic!("Unexpected data: {:?}", e),
                }
            }
        }
    }
}
