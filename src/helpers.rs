use crate::{
    error::{self, Error},
    rpc,
};
use serde::de::DeserializeOwned;

pub fn arbitrary_precision_deserialize_workaround<T>(bytes: &[u8]) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    if cfg!(feature = "arbitrary_precision") {
        // Deserialize with arbitrary precision
        serde_json::from_value(serde_json::from_slice(bytes)?)
    } else {
        // Deserialize without arbitrary precision
        serde_json::from_slice(bytes)
    }
}

pub fn build_request(id: usize, method: &str, params: Vec<rpc::Value>) -> jsonrpc_core::Call {
    rpc::Call::MethodCall(jsonrpc_core::MethodCall {
        jsonrpc: Some(jsonrpc_core::Version::V2),
        id: rpc::Id::Num(id as u64),
        method: method.into(),
        params: rpc::Params::Array(params),
    })
}

pub fn to_result_from_output(output: rpc::Output) -> error::Result<rpc::Value> {
    match output {
        rpc::Output::Success(success) => Ok(success.result),
        rpc::Output::Failure(failure) => Err(error::Error::Rpc(failure.error)),
    }
}

pub fn to_notification_from_slice(notification: &[u8]) -> error::Result<rpc::Notification> {
    serde_json::from_slice(notification)
        .map_err(|e| error::Error::InvalidResponse(format!("{:?}", e)))
}

pub fn to_response_from_slice(response: &[u8]) -> error::Result<rpc::Response> {
    arbitrary_precision_deserialize_workaround(response)
        .map_err(|e| Error::InvalidResponse(format!("{:?}", e)))
}
// Parse a Vec of `rpc::Output` into `Result`.
pub fn to_results_from_outputs(
    outputs: Vec<rpc::Output>,
) -> error::Result<Vec<error::Result<rpc::Value>>> {
    Ok(outputs.into_iter().map(to_result_from_output).collect())
}

// Serializes a request to string. Panics if the type returns error during serialization.
pub fn to_string<T: serde::Serialize>(request: &T) -> String {
    serde_json::to_string(&request).expect("String serialization never fails.")
}
