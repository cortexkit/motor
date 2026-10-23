//! Remote compile cache: one per-tenant object store prefix that every Linux
//! host type reads through a runner-owned gateway. Only the provisioning layer
//! exists so far: it reads the cache tenant's config and keys from disk and
//! decides which complete generation of them is active.
pub(crate) mod provisioning;
