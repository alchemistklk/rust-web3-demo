# WebSocket Transport

The WebSocket transport module implements a persistent, full-duplex JSON-RPC client for Ethereum blockchain communication. Build on the top `soketto`, `tokio` and `futures` libraries, it provides bidirectional communication with comprehensive subscription, management, request correlation, and concurrency message handling capabilities.

## Architecture & Core Components

### Dual-Component Architecture

- Actor Model: Background task handles all I/O operations independently
- Message passing: Communication via unbounded MPSC channels
- Resource Isolation: Network resources managed by dedicated task
- Concurrent Safety: Arc-wrapped atomic counter for request ID generation

The relationship between `WebSocket` and `WebServerTask` follows the Handle-worker pattern. WebSocket is responsible for sending requests and receiving responses, while the background task is responsible for handling the network communication and subscription management. The bridge between the two is the `mpsc::UnboundedSender<TransportMessage>`.


```rust
// 1. WebSocket - Handle
#[derive(Clone, Debug)]
pub struct WebSocket {
    // thread-safe request id generator
    id: Arc<atomic::AtomicUsize>,
    // channel to background task
    requests: mpsc::UnboundedSender<TransportMessage>,
}

// 2. Background Worker - WsServerTask  
struct WsServerTask {
    pending: BTreeMap<RequestId, Pending>,          // Outstanding request tracking
    sender: connection::Sender<MaybeTlsStream<TcpStream, TlsStream>>, // WebSocket sender
    receiver: connection::Receiver<MaybeTlsStream<TcpStream, TlsStream>>, // WebSocket receiver
    subscriptions: BTreeMap<SubscriptionId, Subscription>, // Subscription management
}


// 3. WebSocket acts as a lightweight handle
impl WebSocket {
    pub async fn new(url: &str) -> error::Result<Self> {
        let id = Arc::new(atomic::AtomicUsize::new(1));
        let task = WsServerTask::new(url).await?;           // Create worker
        let (sink, stream) = mpsc::unbounded();             // Create communication channel
        
        tokio::spawn(task.into_task(stream));               // Spawn background worker
        Ok(Self { id, requests: sink })                     // Return lightweight handle
    }
}

```

### I/O Resource Management

1. Connection Establishment & TLS Handling

```rust
impl WsServerTask {
    pub async fn new(url: &str) -> error::Result<Self> {
        // 1.URL parsing and validation
        let url = Url::parse(url)?;
        let scheme = url.scheme();

        // 2. TCP connection establishment
        // use compat compatibility layer to enable interoperability between tokio and futures async ecosystem
        let stream = compat::raw_tcp_stream(addr).await?;

        // 3. TLS wrapping for secure connections
        let socket = if schema == "wss" {
            let tls_stream = async_native_tls::TlsConnector::new()
                .connect(host, stream).await?;
            MaybeTlsStream::Tls(compat::compat(tls_stream))
        } else {
            MaybeTlsStream::Plain(compat::compat(stream))
        };

        // 4. WebSocket handshake
        let mut client = Client::new(socket, host, &resource);
        let (sender, receiver) = match client.handshake().await? {
            // if handshake is successful, return the sender and receiver
            ServerResponse::Accepted { .. } => client.into_builder().finish(),
            ServerResponse::Rejected { status_code } => return Err(TransportError::Code(status_code)),
        };

        // 5. Initialize the task with the sender and receiver, subscriptions are empty by default
        Ok(Self { pending: Default::default(), sender, receiver, subscriptions: Default::default() })
    }
}
```

2. Stream Abstraction with MaybeTlsStream

```rust
enum MaybeTlsStream<P, T> {
    Plain(P),   // Raw TCP stream
    Tls(T),     // TLS-wrapped stream
}

// Unified AsyncRead implementation
impl<P, T> AsyncRead for MaybeTlsStream<P, T>
where P: AsyncRead + AsyncWrite + Unpin, T: AsyncRead + AsyncWrite + Unpin
{
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}
```


### Core Trait Implementations

1. Transport Trait - Single Request/Response

```rust
// 1. Single Request/Response
impl Transport for WebSocket {
    // Define the return type containing (final result, extractor)
    // This approach centralizes the async I/O oneshot channel handling, and scheduling logic
    type Out = Response<rpc::Value, fn(BatchResult) -> SingleResult>;

    fn prepare(&self, method: &str, params: Vec<Value>) -> (RequestId, Call) {
        // Generate a new request ID 
        let id = self.id.fetch_add(1, atomic::Ordering::AcqRel);
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, request: Call) -> Self::Out {
        let response = self.send_request(id, rpc::Request::Single(request));
        Response::new(response, batch_to_single) // Transform batch result to single
    }
}

// 2. Send Request
fn send_request(
        &self,
        id: RequestId,
        request: rpc::Request,
    ) -> error::Result<oneshot::Receiver<BatchResult>> {
        // 1. Serialize the request to string
        let request = helpers::to_string(&request);
        log::debug!("[{}] Calling: {}", id, request);

        // 2. Create a oneshot channel to receive the response
        let (sender, receiver) = oneshot::channel();

        // 3. Send the request to the background task
        self.send(TransportMessage::Request {
            id,
            request,
            sender,
        });
        Ok(receiver)
    }

```

2. BatchTransport Trait - Batch Request/Response

```rust

impl BatchTransport for WebSocket {
    // Define the return type containing multiple results
    type Batch = Response<Vec<SingleResult>, fn(BatchResult) -> BatchResult>;
    
    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where T: IntoIterator<Item = (RequestId, Call)>
    {
        let mut it = requests.into_iter();
        let (id, first) = it.next().map(|x| (x.0, Some(x.1))).unwrap_or_else(|| (0, None));
        let requests = first.into_iter().chain(it.map(|x| x.1)).collect();
        
        let response = self.send_request(id, rpc::Request::Batch(requests));
        Response::new(response, batch_to_batch) // Identity transformation
    }
}

```

3. Subscription Trait - Subscription Management

```rust
impl DuplexTransport for WebSocket {
    type NotificationStream = mpsc::UnboundedReceiver<rpc::Value>;

    fn subscribe(&self, id: SubscriptionId) -> error::Result<Self::NotificationStream> {
        // sink is a channel to send notifications to the client
        // stream is a channel to receive notifications from the client
        let (sink, stream) = mpsc::unbounded();
        self.send(TransportMessage::Subscribe { id, sink });
        Ok(stream)
    }

    fn unsubscribe(&self, id: SubscriptionId) -> error::Result<()> {
        self.send(TransportMessage::Unsubscribe { id });
        Ok(())
    }
}

```

### Background Task Event Loop

1. Core Event Loop

```rust
async fn into_task(self, requests: mpsc::UnboundedReceiver<TransportMessage>) {
    let Self { receiver, mut sender, mut pending, mut subscriptions } = self;
    // transform the soketto receiver into a stream of data
    // fuse the receiver and requests to handle the channel closing
    let receiver = as_data_stream(receiver).fuse();
    // fuse the requests to handle the channel closing
    let requests = requests.fuse();
    // pin receiver and request 
    pin_mut!(receiver, requests);

    // loop through the requests and handle the messages from the WebSocket
    loop {
        select! {
            // Handle outgoing requests from application
            msg = requests.next() => match msg {
                Some(TransportMessage::Request { id, request, sender: tx }) => {
                    pending.insert(id, tx);                    // Store response channel
                    let _ = sender.send_text(request).await;   // Send over WebSocket
                    let _ = sender.flush().await;              // Ensure transmission
                }
                Some(TransportMessage::Subscribe { id, sink }) => {
                    subscriptions.insert(id, sink);           // Register subscription
                }
                Some(TransportMessage::Unsubscribe { id }) => {
                    subscriptions.remove(&id);                // Cleanup subscription
                }
                None => break, // Channel closed, terminate task
            },
            
            // Handle incoming messages from WebSocket
            res = receiver.next() => match res {
                Some(Ok(data)) => {
                    handle_message(&data, &subscriptions, &mut pending);
                }
                Some(Err(e)) => {
                    log::error!("WebSocket error: {}", e);
                    break; // Connection error, terminate
                }
                None => break, // Connection closed
            },
        }
    }
}

```

2. Message Routing & Correlation

The `handle_message` function is responsible for routing the messages to the appropriate subscribers and correlating the responses to the requests.

```rust
fn handle_message(
    data: &[u8],
    subscriptions: &BTreeMap<SubscriptionId, Subscription>,
    pending: &mut BTreeMap<RequestId, Pending>,
) {
    // 1. Try parsing as notification first
    if let Ok(notification) = helpers::to_notification_from_slice(data) {
        if let rpc::Params::Map(params) = notification.params {
            if let (Some(&rpc::Value::String(ref id)), Some(result)) = 
                (params.get("subscription"), params.get("result")) {
                
                let id: SubscriptionId = id.clone().into();
                if let Some(stream) = subscriptions.get(&id) {
                    let _ = stream.unbounded_send(result.clone()); // Route to subscriber
                }
            }
        }
        return;
    }
    
    // 2. Parse as RPC response
    let response = helpers::to_response_from_slice(data);
    let output = match response {
        Ok(rpc::Response::Single(output)) => vec![output],
        Ok(rpc::Response::Batch(outputs)) => outputs,
        _ => return,
    };
    
    // 3. Extract request ID and correlate
    if let Some(first_output) = output.get(0) {
        let id = match first_output {
            rpc::Output::Success(success) => &success.id,
            rpc::Output::Failure(failure) => &failure.id,
        };
        
        if let rpc::Id::Num(num) = id {
            if let Some(sender) = pending.remove(&(*num as usize)) {
                let _ = sender.send(helpers::to_results_from_outputs(output));
            }
        }
    }
}
```