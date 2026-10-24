#!/usr/bin/env python3
"""The job environment a remote compile cache adds on each runner host type.

The runner (crates/motor-runner) and the toolchain parity check must agree on
these names and values, so both read them from here. The parity check expands
``<forwarder-port:sccache>`` to the tool's published port: a real job has a port number
there, chosen by the runner's forwarder configuration.

Only values that do not depend on the cache tenant's provisioning files live
here. The store bucket, region, endpoint and credentials are written into unit
environments by the runner from the active provisioning generation, and are
never pinned in this repository.
"""

# Names the runner removes from a job environment before it adds anything: on
# every host type, sccache settings inherited from whatever launched the
# runner (or from a caller request, whatever the allowlist let through) must
# not shape either the local level-1 settings or the remote ones. The fill
# side additionally clears these for its own unit environment.
INHERITED_PREFIXES = ('SCCACHE_', 'AWS_')

# Values the fill units use and jobs never see.
READ_WRITE_MODE = 'READ_WRITE'
READ_ONLY_MODE = 'READ_ONLY'
# Under this policy every put waits for every read-write level and fails if
# any fails, so a fill learns about a remote write failure from sccache's own
# statistics rather than after its statistics were captured.
WRITE_ERROR_POLICY = 'all'

# sccache has no storage-class setting in the pinned release, so the runner's
# write endpoint adds the header; the tool must not be able to contradict it.
_S3_COMMON = {
    'SCCACHE_S3_USE_SSL': 'false',
    'SCCACHE_S3_ENABLE_VIRTUAL_HOST_STYLE': 'false',
    # The runner's gateway signs every request, so the tool uses fixed,
    # non-secret credentials. sccache refuses to combine S3_NO_CREDENTIALS
    # with credentials, and that option does not give read-only behaviour.
    'AWS_ACCESS_KEY_ID': 'remote-cache',
    'AWS_SECRET_ACCESS_KEY': 'remote-cache',
    # Without this the SDK tries the instance metadata service first.
    'AWS_EC2_METADATA_DISABLED': 'true',
}

# The bucket, region and signing endpoint of a job's read socket. The bucket
# and region come from the active provisioning generation in a real unit.
S3_FRONT_END = {
    'SCCACHE_BUCKET': '<remote-cache-bucket>',
    'SCCACHE_REGION': '<remote-cache-region>',
    'SCCACHE_ENDPOINT': 'http://127.0.0.1:<forwarder-port:sccache>',
}

# The job environment on each host type with the remote cache switched on,
# expressed as the switch-off baseline plus this delta.
JOB_ENVIRONMENT_BY_ROLE = {
    # The read-only /motor-cache stays sccache's level 1 and the store is level
    # 2. Every job setting keeps its baseline value: SCCACHE_DIR,
    # SCCACHE_CACHE_SIZE, SCCACHE_LOCAL_RW_MODE=READ_ONLY and
    # SCCACHE_IGNORE_SERVER_IO_ERROR come from compile_cache::reader_environment.
    'build-server': {
        **S3_FRONT_END,
        **_S3_COMMON,
        'SCCACHE_S3_RW_MODE': READ_ONLY_MODE,
        'SCCACHE_MULTILEVEL_CHAIN': 'disk,s3',
    },
    # Boat and tenant VMs have no level 1, so there is no chain: sccache talks
    # to the store only. The runner installs its trusted wrapper as
    # RUSTC_WRAPPER because these jobs otherwise have none.
    'boat': {
        **S3_FRONT_END,
        **_S3_COMMON,
        'SCCACHE_S3_RW_MODE': READ_ONLY_MODE,
        'RUSTC_WRAPPER': '/motor-tools/sccache',
    },
    'tenant': {
        **S3_FRONT_END,
        **_S3_COMMON,
        'SCCACHE_S3_RW_MODE': READ_ONLY_MODE,
        'RUSTC_WRAPPER': '/motor-tools/sccache',
    },
}

# Ports the runner publishes its endpoint on, as seen by a job: the runner
# configuration's forwarder_ports map. Tool ids are the manifest's; a tool
# without a port cannot be exposed to a job that needs a URL.
FORWARDER_PORTS = {
    'sccache': 4181,
    'ccache': 4182,
}

# The placeholder a role's value may use for the tool's forwarder URL.
FORWARDER_PLACEHOLDER = '<forwarder-port:'


def expand(value, ports=None):
    """Replace ``<forwarder-port:TOOL>`` in `value` with TOOL's published port."""
    ports = FORWARDER_PORTS if ports is None else ports
    prefix, suffix = FORWARDER_PLACEHOLDER, '>'
    out = value
    while prefix in out:
        start = out.index(prefix)
        end = out.index(suffix, start)
        tool = out[start + len(prefix):end]
        out = out[:start] + str(ports[tool]) + out[end + len(suffix):]
    return out


def environment(role, ports=None):
    """One role's switch-on job environment delta, fully expanded."""
    return {name: expand(value, ports) for name, value in JOB_ENVIRONMENT_BY_ROLE[role].items()}
