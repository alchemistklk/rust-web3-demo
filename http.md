# Http Transport

The HTTP transport module implements a stateless, request-response `JSON-RPC` client for Ethereum blockchain communication.Built on the top of `reqwest` and `tokio` libraries, it provides high-performance and async communication with comprehensive core error handling and batch processing capabilities.

## Architecture & Core Components

### Primary Structure

```rust
#[derive(Clone, Debug)]
pub struct Http {
    client: Client,      // reqwest HTTP client (already Arc-wrapped)
    inner: Arc<Inner>,   // Shared state across clones
}

// shared state across clonesl
struct Inner {
    url: Url,                    // Target RPC endpoint
    id: AtomicUsize,            // Thread-safe request ID generator
}   

```

Keys Design Principles

- Stateless Architecture: Each request is independent, no persistent connection
- Shared Ownership: `Arc<Inter>` enables safe cloning across threads
- Zero-Copy Cloning: `reqwest::Client` is internally Arc-wrapped for efficient cloning


### Core Trait Implementations

1. Transport Trait - Single Request/Response Functionality

- Future Type: BoxFuture<'static, Result<Value>> for type erasure and heap allocation
- Request Preparation: Atomic ID generation with JSON-RPC request building
- Async Execution: Non-blocking HTTP requests with comprehensive error handling

```rust
impl Transport for Http {
    // Define the return type for the send method including lifetime and memory allocation
    type Out = BoxFuture<'static, Result<Value>>;

    // Prepare method: Generate a unique request ID and build the JSON-RPC request using jsonrpc_core::Method Call
    fn prepare(&self, method: &str, params: Vec<Value>) -> (RequestId, Call) {
        let id = self.next_id();
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    // every time we call send method, we will create a new ulr and increase the ref count of the client, because it has wrapped in Arc, so the clone is cheap 
    fn send(&self, id: RequestId, call: Call) -> Self::Out {
        let (client, url) = self.new_request();
    
        // Box::pin is used to pin the future on the heap to avoid the undefined error while moving them
        Box::pin(async move {
            // execute single request asynchronously
            let out = execute_rpc(&client, url, &Request::Single(call), id).await?;
            // serialize the output to result which type has been declared at the first,
            // the result is wrapped in Result<Value>
            helpers::to_result_from_output(out).map_err(|err| {
                Error::Transport(TransportError::Message(format!(
                    "failed to convert output to result: {}", err
                )))
            })
        })
    }
}

```

2. BatchTransport Trait - Batch Request/Response Functionality

- Network Efficiency: Single HTTP request for multiple RPC calls
- Reduced Latency: Eliminates multiple round trips
- Order Preservation: Responses maintain original request sequence
- Error Isolation: Individual failures do not affect other requests


```rust
impl BatchTransport for Http {
    // Define multi returned value are collected in a vector
    type Batch = BoxFuture<'static, Result<Vec<Result<Value>>>>;
    
    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, Call)>,
    {
        // using the id
        let id = self.next_id();
        let (client, url) = self.new_request();
        // unzip the requests into ids and calls
        let (ids, calls): (Vec<_>, Vec<_>) = requests.into_iter().unzip();

        Box::pin(async move {
            let value = execute_rpc(&client, url, &Request::Batch(calls), id).await?;
            let outputs = handle_possible_error_object_for_batched_request(value)?;
            handle_batch_response(&ids, outputs)
        })
    }
}

```


## Request Management

```rust
fn next_id(&self) -> RequestId {
    self.inner.id.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
}
```
## Response Handling Pattern

### Single Response Processing

```rust
let out = execute_rpc(&client, url, &Request::Single(call), id).await?;
helpers::to_result_from_output(out)
```

### Batch Response Processing

```rust
fn handle_batch_response(ids: &[RequestId], outputs: Vec<Output>) -> Result<Vec<RpcResult>> {
    // 1. Validate response count
    if ids.len() != outputs.len() {
        return Err(Error::InvalidResponse("unexpected number of responses".to_string()));
    }
    
    // 2. Build ID->Response mapping for correlation
    let mut outputs = outputs
        .into_iter()
        .map(|output| Ok((id_of_output(&output)?, helpers::to_result_from_output(output))))
        .collect::<Result<HashMap<_, _>>>()?;
    
    // 3. Return responses in original request order
    ids.iter()
        .map(|id| outputs.remove(id).ok_or_else(|| {
            Error::InvalidResponse(format!("batch response is missing id {}", id))
        }))
        .collect()
}
```

## Json-RPC Protocol Handling

### Request Building

```rust
fn prepare(&self, method: &str, params: Vec<Value>) -> (RequestId, Call) {
    let id = self.next_id();
    let request = helpers::build_request(id, method, params);
    (id, request)
}
```

### Response Parsing

Single Response Parsing

```rust
// Deserialize with precision handling
helpers::arbitrary_precision_deserialize_workaround(&response).map_err(|err| {
    Error::Transport(TransportError::Message(format!(
        "failed to deserialize response: {}: {}",
        err,
        String::from_utf8_lossy(&response)
    )))
})
```

Batch Response Parsing

```rust
fn handle_possible_error_object_for_batched_request(value: Value) -> Result<Vec<Output>> {
    if value.is_object() {
        // Handle single error response for entire batch
        let output: Output = serde_json::from_value(value)?;
        return Err(match output {
            Output::Failure(failure) => Error::Rpc(failure.error),
            Output::Success(_) => Error::InvalidResponse("Invalid response for batched request".to_string()),
        });
    }
    // Handle normal batch response array
    serde_json::from_value(value).map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to deserialize batch response: {}", err
        )))
    })
}

```

## Connection Management

### Stateless Architecture

```rust
fn new_request(&self) -> (Client, Url) {
    (self.client.clone(), self.inner.url.clone())
}

```

### Client Configuration

```rust
pub fn new(url: &str) -> Result<Self> {
    let mut builder = reqwest::Client::builder();
    
    #[cfg(not(feature = "wasm"))]
    {
        let value = reqwest::header::HeaderValue::from_static("web3.rs");
        builder = builder.user_agent(value);
    }

    let client = builder.build().map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to build client: {}", err
        )))
    })?;
    
    Ok(Self::with_client(client, url.parse()?))
}

```

### execute_rpc - The heart of HTTP communication

The `execute_rpc` function is the central method that handles all HTTP-based JSON-RPC communication. It encapsulates the complete request-response lifecycle with comprehensive error handling and logging.


```rust
// T: DeserializeOwned is a trait bound that ensures the type T can be deserialized from JSON, allows different return types
async fn execute_rpc<T: DeserializeOwned>(
    client: &Client,
    url: Url,
    request: &Request,
    id: RequestId,
) -> Result<T> {
    // log the request
    log::debug!(
        "[id:{}], sending request: {:?}, request",
        id,
        serde_json::to_string(request)
    );

    // send the request to the server
    let response = client.post(url).json(request).send().await.map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to send request: {}",
            err
        )))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Transport(TransportError::Code(status.as_u16())));
    }
    // read the response from the server
    let response = response.bytes().await.map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to read response: {}",
            err
        )))
    })?;
    log::debug!(
        "[id:{}], received response: {:?}",
        id,
        String::from_utf8_lossy(&response)
    );

    // Deserialize the response
    // this is a workaround to handle the precision issue of the response
    helpers::arbitrary_precision_deserialize_workaround(&response).map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to deserialize response: {}: {}",
            err,
            String::from_utf8_lossy(&response)
        )))
    })
}
```

## High Concurrency With Tokio and Futures

### Traditional HTTP Model vs Async HTTP Model

1. Traditional HTTP Model
- Blocking I/O: Each request blocks a thread until completion
- Context Switching: Each request requires a new thread, leading to high overhead

```rust
fn traditional_http_request(url: &str, payload: &str) -> Result<String> {
    let mut stream = TcpStream::connect("server:8545")?;  // Blocks thread
    stream.write_all(payload.as_bytes())?;               // Blocks thread
    
    let mut response = String::new();
    stream.read_to_string(&mut response)?;               // Blocks thread
    Ok(response)
}

// Each request requires a dedicated thread
fn handle_multiple_requests() {
    for request in requests {
        thread::spawn(move || {
            let result = traditional_http_request(&url, &payload);
            // Thread blocked until completion
        });
    }
}

```

2. Async HTTP Model

- Non-blocking I/O: Requests yield control to the event loop
- Concurrent Execution: Multiple requests can run concurrently
- Scalability: Handles thousands of concurrent requests efficiently

```rust
// Async HTTP with Tokio (our implementation)
async fn async_http_request(client: &Client, url: Url, request: &Request) -> Result<Value> {
    let response = client.post(url).json(request).send().await?;  // Yields control
    let bytes = response.bytes().await?;                          // Yields control
    serde_json::from_slice(&bytes)                               // CPU work
}

// Single thread handles thousands of concurrent requests
async fn handle_multiple_requests() {
    let futures: Vec<_> = requests.into_iter()
        .map(|req| async_http_request(&client, url, &req))
        .collect();
    
    let results = futures::future::join_all(futures).await;  // Concurrent execution
}
```

3. Tokio and Futures

- Event-driven Architecture: Non-blocking I/O with a single thread
- Concurrency Model: Tokio's runtime manages thousands of concurrent tasks, work-stealing scheduler optimizes CPU utilization
- Asynchronous Execution: Efficient handling of thousands of requests

```rust
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let transport = Http::new("http://localhost:8545").unwrap();
    
    // Spawn tasks across worker threads
    let handles: Vec<_> = (0..1000).map(|i| {
        let transport = transport.clone();  // Cheap Arc clone
        tokio::spawn(async move {
            transport.execute("eth_blockNumber", vec![]).await
        })
    }).collect();
    
    let results = futures::future::join_all(handles).await;
}
```