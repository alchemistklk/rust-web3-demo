use crate::rpc::error::Error as RPCError;
use derive_more::{Display, From};
pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(Display, Debug, Clone, PartialEq)]
pub enum TransportError {
    #[display(fmt = "code {}", _0)]
    Code(u16),
    #[display(fmt = "message {}", _0)]
    Message(String),
}

#[derive(Display, Debug, From, Clone, PartialEq)]
pub enum Error {
    // invalid response
    #[display(fmt = "Got invalid response: {}", _0)]
    #[from(ignore)]
    InvalidResponse(String),
    #[display(fmt = "Transport error: {}", _0)]
    #[from(ignore)]
    Transport(TransportError),

    // rpc error
    #[display(fmt = "RPC error: {:?}", _0)]
    Rpc(RPCError),
}
