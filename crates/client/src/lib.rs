mod manager;
mod model;
mod server;

pub use comet_proto::ServerRef;
pub use manager::{Federation, FederationError, RemoteConnectError, RemoteConnector};
pub use model::{
    FederationCommand, FederationEvent, FederationStream, ServerSnapshot, ServerState,
};
