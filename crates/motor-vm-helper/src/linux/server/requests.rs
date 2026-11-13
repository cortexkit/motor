use crate::{job, Request};

// Keep the wire enum unchanged while routing job work outside the Helper lock.
pub(super) fn job_request(request: Request) -> Result<job::Operation, Request> {
    Ok(match request {
        Request::StartJob {
            kind,
            job_id,
            account,
            workspace_disk_id,
            image_generation_id,
            vcpus,
            memory_mib,
            burst_mib,
            cache_write_blocked,
        } => job::Operation::Start(job::Start {
            kind,
            job_id,
            account,
            workspace_disk_id,
            image_generation_id,
            vcpus,
            memory_mib,
            burst_mib,
            cache_write_blocked,
        }),
        Request::AdoptJob { job_id } => job::Operation::Adopt(job_id),
        Request::StopJob { job_id } => job::Operation::Stop(job_id),
        Request::JobMemory { job_id } => job::Operation::Memory(job_id),
        Request::BalloonJob { job_id, amount_mib } => job::Operation::Balloon(job_id, amount_mib),
        Request::OpenJobRelay { job_id, port } => job::Operation::Relay(job_id, port),
        Request::JobExits {} => job::Operation::Exits,
        Request::DeleteWorkspace { account, disk_id } => {
            job::Operation::DeleteWorkspace { account, disk_id }
        }
        Request::PrepareFill { fill } => job::Operation::PrepareFill(fill),
        Request::FinishFill { fill, published } => job::Operation::FinishFill { fill, published },
        Request::SeedWorkspace { seed } => job::Operation::SeedWorkspace(seed),
        Request::EvictBase { account, disk_id } => job::Operation::EvictBase { account, disk_id },
        Request::WorkspaceUsage { account, disk_id } => {
            job::Operation::WorkspaceUsage { account, disk_id }
        }
        Request::EnsureWorkspace {
            account,
            disk_id,
            size_bytes,
            cache_write_job_id,
        } => job::Operation::EnsureWorkspace {
            account,
            disk_id,
            size_bytes,
            cache_write_job_id,
        },
        Request::AckJobExit {
            job_id,
            oom_kill,
            egress_bytes,
        } => job::Operation::AckExit(job::ExitReport {
            job_id,
            oom_kill,
            egress_bytes,
        }),
        request => return Err(request),
    })
}
