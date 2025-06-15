use jsonrpc_core as rpc;

pub mod api;
pub mod error;
pub mod helpers;
pub mod transports;

pub type RequestId = usize;

pub trait Transport: std::fmt::Debug + Clone {
    // define a output for the transport
    type Out: futures::Future<Output = error::Result<rpc::Value>>;

    // prepare serializable rpc call for given method
    fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call);

    // send the rpc call to the transport
    fn send(&self, id: RequestId, request: rpc::Call) -> Self::Out;

    fn execute(&self, method: &str, params: Vec<rpc::Value>) -> Self::Out {
        let (id, request) = self.prepare(method, params);
        self.send(id, request)
    }
}


// a transport implementation supporting batch request
pub trait BatchTransport: Transport {
    type Batch: futures::Future<Output = error::Result<Vec<error::Result<rpc::Value>>>>;

    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, rpc::Call)>;
}


// A transport that implements supporting pub and sub subscriptions
pub trait DuplexTransport: Transport {
    /// The type of stream this transport returns
    type NotificationStream: futures::Stream<Item = rpc::Value>;

    /// Add a subscription to this transport
    fn subscribe(&self, id: api::SubscriptionId) -> error::Result<Self::NotificationStream>;

    /// Remove a subscription from this transport
    fn unsubscribe(&self, id: api::SubscriptionId) -> error::Result<()>;
}