use super::{io, Storage, Uuid, NO_HOST_FITS};

pub(crate) enum DirectRoute {
    SpillReturn {
        runner_job_id: Uuid,
        hosted_code: &'static str,
    },
    NoHostFits {
        max_vcpus: Option<u64>,
    },
}

/// Reproduce direct-route records written by older ck-motor versions so tests
/// exercise the shipped compatibility readers, not the retired routing policy.
pub(crate) fn seed_direct_route(storage: &Storage, id: Uuid, route: DirectRoute) -> io::Result<()> {
    storage.update(|state| {
        let job = state.jobs.get_mut(&id).expect("legacy fixture job exists");
        match route {
            DirectRoute::SpillReturn {
                runner_job_id,
                hosted_code,
            } => {
                job.build_server_job_id = Some(runner_job_id);
                if let Some(hosted) = job.hosted.as_mut() {
                    hosted.hosted_code = Some(hosted_code.into());
                }
            }
            DirectRoute::NoHostFits { max_vcpus } => {
                job.hosted = None;
                job.routed_direct_reason = Some(NO_HOST_FITS.into());
                job.routed_direct_hosted_max_vcpus = max_vcpus;
            }
        }
    })
}
