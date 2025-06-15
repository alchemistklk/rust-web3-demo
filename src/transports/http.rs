use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{BatchTransport, RequestId, Transport, error::TransportError};
use crate::{
    error::{Error, Result},
    helpers,
};
use futures::future::BoxFuture;

use jsonrpc_core::Request;
use jsonrpc_core::types::Value;
use jsonrpc_core::{Call, Output};
use reqwest::Client;
use serde::de::DeserializeOwned;
use url::Url;

#[derive(Clone, Debug)]
pub struct Http {
    // client is already arc so don't need to put it into Arc
    client: Client,
    inner: Arc<Inner>,
}
#[derive(Debug)]
struct Inner {
    url: Url,
    id: AtomicUsize,
}

impl Http {
    // Create a new Http transport to given url
    pub fn new(url: &str) -> Result<Self> {
        let mut builder = reqwest::Client::builder();
        // conditional compilation attribute, WASM/BROWSER typically control user-agent headers
        #[cfg(not(feature = "wasm"))]
        {
            // set client's user agent
            let value = reqwest::header::HeaderValue::from_static("web3.rs");
            builder = builder.user_agent(value);
        }

        let client = builder.build().map_err(|err| {
            Error::Transport(TransportError::Message(format!(
                "failed to build client: {}",
                err
            )))
        })?;
        Ok(Self::with_client(
            client,
            url.parse().map_err(|err| {
                Error::Transport(TransportError::Message(format!(
                    "failed to parse url: {}",
                    err
                )))
            })?,
        ))
    }

    // Create a new Http transport with a custom client and url
    pub fn with_client(client: Client, url: Url) -> Self {
        let inner = Arc::new(Inner {
            url,
            id: AtomicUsize::new(0),
        });
        Self { client, inner }
    }

    fn next_id(&self) -> RequestId {
        // increment the id atomically
        self.inner.id.fetch_add(1, Ordering::AcqRel)
    }

    fn new_request(&self) -> (Client, Url) {
        (self.client.clone(), self.inner.url.clone())
    }
}

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
    helpers::arbitrary_precision_deserialize_workaround(&response).map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to deserialize response: {}: {}",
            err,
            String::from_utf8_lossy(&response)
        )))
    })
}

type RpcResult = Result<Value>;

impl Transport for Http {
    // Define the output type for the transport
    // 1. assign it to heap, isolating lifetime
    // 2. BoxFuture is a type alias for a boxed future
    // 3. 'static lifetime means the future can live for the entire duration of the program
    // 4. Result<Value> is the result type of the future
    type Out = BoxFuture<'static, Result<Value>>;

    fn prepare(&self, method: &str, params: Vec<Value>) -> (RequestId, Call) {
        let id = self.next_id();
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, call: Call) -> Self::Out {
        let (client, url) = self.new_request();

        // Create a future that executes the RPC call
        // and returns a boxed future
        // Box::pin is used to pin the future on the heap
        Box::pin(async move {
            let out = execute_rpc(&client, url, &Request::Single(call), id).await?;
            helpers::to_result_from_output(out).map_err(|err| {
                Error::Transport(TransportError::Message(format!(
                    "failed to convert output to result: {}",
                    err
                )))
            })
        })
    }
}

impl BatchTransport for Http {
    type Batch = BoxFuture<'static, Result<Vec<Result<Value>>>>;
    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, Call)>,
    {
        let id = self.next_id();
        let (client, url) = self.new_request();
        // Convert the requests into a vector of (id, call) tuples
        let (ids, calls): (Vec<_>, Vec<_>) = requests.into_iter().unzip();

        // Create a future that executes the batch RPC call
        Box::pin(async move {
            let value = execute_rpc(&client, url, &Request::Batch(calls), id).await?;
            let outputs = handle_possible_error_object_for_batched_request(value)?;
            handle_batch_response(&ids, outputs)
        })
    }
}

fn handle_possible_error_object_for_batched_request(value: Value) -> Result<Vec<Output>> {
    if value.is_object() {
        let output: Output = serde_json::from_value(value).map_err(|err| {
            Error::Transport(TransportError::Message(format!(
                "failed to deserialize error object: {}",
                err
            )))
        })?;
        return Err(match output {
            Output::Failure(failure) => Error::Rpc(failure.error),
            Output::Success(success) => {
                // totally unlikely - we got json success object for batched request
                Error::InvalidResponse(format!(
                    "Invalid response for batched request: {:?}",
                    success
                ))
            }
        });
    }
    let outputs = serde_json::from_value(value).map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to deserialize batch response: {}",
            err
        )))
    })?;
    Ok(outputs)
}

fn handle_batch_response(ids: &[RequestId], outputs: Vec<Output>) -> Result<Vec<RpcResult>> {
    if ids.len() != outputs.len() {
        return Err(Error::InvalidResponse(
            "unexpected number of responses".to_string(),
        ));
    }
    let mut outputs = outputs
        .into_iter()
        .map(|output| {
            Ok((
                id_of_output(&output)?,
                helpers::to_result_from_output(output),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    ids.iter()
        .map(|id| {
            outputs.remove(id).ok_or_else(|| {
                Error::InvalidResponse(format!("batch response is missing id {}", id))
            })
        })
        .collect()
}

fn id_of_output(output: &Output) -> Result<RequestId> {
    let id = match output {
        Output::Success(success) => &success.id,
        Output::Failure(failure) => &failure.id,
    };
    match id {
        jsonrpc_core::Id::Num(num) => Ok(*num as RequestId),
        _ => Err(Error::InvalidResponse("response id is not u64".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    use crate::{Transport, transports::http::Http};

    fn get_available_port() -> Option<u16> {
        Some(
            TcpListener::bind(("127.0.0.1", 0))
                .ok()?
                .local_addr()
                .ok()?
                .port(),
        )
    }

    async fn server(
        req: hyper::Request<hyper::body::Incoming>,
    ) -> hyper::Result<hyper::Response<Full<Bytes>>> {
        let expected = r#"{"jsonrpc":"2.0","method":"eth_getAccounts","params":[],"id":0}"#;
        let response = r#"{"jsonrpc":"2.0","id":0,"result":"x"}"#;

        assert_eq!(req.method(), &hyper::Method::POST);
        assert_eq!(req.uri().path(), "/");
        let mut content: Vec<u8> = vec![];
        let body = req.into_body();
        let body = body.collect().await?.to_bytes().to_vec();
        content.extend(body);

        assert_eq!(std::str::from_utf8(&*content), Ok(expected));

        Ok(hyper::Response::new(Full::new(Bytes::from(response))))
    }

    #[tokio::test]
    async fn should_make_a_request() {
        use hyper::service::service_fn;
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use serde_json::Value;
        use tokio::net::TcpListener;
        // given
        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let addr_clone = addr.clone();
        // start server
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            println!("Listening on http://{}", addr_clone);
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service_fn(server))
                .await
                .unwrap();
        });

        // when
        let client = Http::new(&format!("http://{}", &addr)).unwrap();
        println!("Sending request");
        let response = client.execute("eth_getAccounts", vec![]).await;
        println!("Got response");

        // then
        assert!(response.is_ok());
        assert_eq!(response.unwrap(), Value::String("x".into()));
    }
}
