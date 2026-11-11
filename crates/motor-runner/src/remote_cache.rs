//! Remote compile cache: one per-tenant object store prefix that every Linux
//! host type reads through a runner-owned gateway. The provisioning layer
//! reads the cache tenant's config and keys from disk and decides which
//! complete generation of them is active; the gateway serves tools over Unix
//! sockets and signs their requests with that generation; the transport
//! creates each unit's sockets and the forwarders that expose them inside the
//! unit's sandbox. The runtime starts them only for an isolated, opted-in runner.
//! Records and publication policy are shared across platforms; provisioning
//! file access, gateways and sandbox transports are Unix-only.
pub(crate) const DEFAULT_JOB_PUT_OBJECTS: u64 = 5_000;
pub(crate) const DEFAULT_JOB_PUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const DEFAULT_JOB_PUTS: usize = 16;

#[cfg(unix)]
pub(crate) mod ccache;
#[cfg(unix)]
pub(crate) mod environment;
#[cfg(unix)]
pub(crate) mod gateway;
#[cfg(unix)]
pub(crate) mod job_totals;
pub(crate) mod provisioning;
pub(crate) mod publication;
pub(crate) mod publishing;
#[cfg(unix)]
pub(crate) mod runtime;
#[cfg(unix)]
pub(crate) mod transport;
