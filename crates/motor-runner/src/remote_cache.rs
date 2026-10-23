//! Remote compile cache: one per-tenant object store prefix that every Linux
//! host type reads through a runner-owned gateway. The provisioning layer
//! reads the cache tenant's config and keys from disk and decides which
//! complete generation of them is active; the gateway serves tools over Unix
//! sockets and signs their requests with that generation. Nothing in the
//! runner starts either yet.
pub(crate) mod gateway;
pub(crate) mod provisioning;
