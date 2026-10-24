//! Remote compile cache: one per-tenant object store prefix that every Linux
//! host type reads through a runner-owned gateway. The provisioning layer
//! reads the cache tenant's config and keys from disk and decides which
//! complete generation of them is active; the gateway serves tools over Unix
//! sockets and signs their requests with that generation; the transport
//! creates each unit's sockets and the forwarders that expose them inside the
//! unit's sandbox. The runtime starts them only for an isolated, opted-in runner.
pub(crate) mod ccache;
pub(crate) mod environment;
pub(crate) mod gateway;
pub(crate) mod provisioning;
pub(crate) mod publication;
pub(crate) mod publishing;
pub(crate) mod runtime;
pub(crate) mod transport;
