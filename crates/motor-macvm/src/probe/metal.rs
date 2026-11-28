//! Compare job VMs, which have no window or login, with and without their
//! paravirtual GPU.

use crate::{JobVmConfig, JobVmRequest};
use std::io;
use std::path::{Path, PathBuf};

/// The two cases use the same job helper and differ only in the graphics device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Graphics,
    Control,
}

impl Mode {
    pub fn parse(name: &str) -> io::Result<Self> {
        match name {
            "graphics" => Ok(Self::Graphics),
            "control" => Ok(Self::Control),
            _ => Err(super::invalid(
                "Metal probe mode must be graphics or control",
            )),
        }
    }

    pub fn plan(self, request: JobVmRequest) -> io::Result<JobVmConfig> {
        let plan = JobVmConfig::new(request)?;
        Ok(match self {
            Self::Graphics => plan,
            Self::Control => plan.with_probe_without_graphics(),
        })
    }

    #[cfg(target_os = "macos")]
    fn name(self) -> &'static str {
        match self {
            Self::Graphics => "graphics",
            Self::Control => "control",
        }
    }
}

/// The job helper records the time beside the clone, outside both guest shares.
pub fn timing_path(clone: &Path) -> PathBuf {
    clone.with_extension("hello-us")
}

#[cfg(any(test, target_os = "macos"))]
fn arguments<'a>(args: &[&'a str]) -> io::Result<(&'a Path, &'a Path)> {
    match args {
        [image, work]
            if [image, work]
                .iter()
                .all(|path| crate::absolute_normal(Path::new(path))) =>
        {
            Ok((Path::new(*image), Path::new(*work)))
        }
        [_, _] => Err(super::invalid(
            "image and work directory must be normalized absolute paths",
        )),
        _ => Err(super::invalid(
            "usage: motor-macvm metal-probe <sealed-image> <new-work-directory>",
        )),
    }
}

#[cfg(any(test, target_os = "macos"))]
fn work_directory(image: &Path, work: &Path) -> io::Result<PathBuf> {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let parent = fs::canonicalize(work.parent().expect("normalized absolute path"))?;
    let image = fs::metadata(image)?;
    for ancestor in parent.ancestors() {
        let metadata = fs::metadata(ancestor)?;
        // Compare the directories even when the caller uses a symlink or the
        // volume treats differently cased names as the same path.
        if (metadata.dev(), metadata.ino()) == (image.dev(), image.ino()) {
            return Err(super::invalid(
                "the work directory must not be inside the image",
            ));
        }
        // The runner keeps build disks in targets/ and helper copies in tools/.
        if ancestor.join("targets").is_dir() && ancestor.join("tools").is_dir() {
            return Err(super::invalid(format!(
                "{} looks like a runner state root; choose a work directory outside it",
                ancestor.display()
            )));
        }
    }
    let work = parent.join(work.file_name().expect("normalized absolute path"));
    fs::DirBuilder::new().mode(0o700).create(&work)?;
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metal_probe_requires_two_normalized_absolute_paths() {
        assert_eq!(
            arguments(&["/sealed image", "/scratch/new probe"]).unwrap(),
            (Path::new("/sealed image"), Path::new("/scratch/new probe")),
        );
        for args in [
            &[][..],
            &["/image"],
            &["/image", "/work", "extra"],
            &["image", "/work"],
            &["/image", "/state/../work"],
            &["/image", "/"],
        ] {
            assert!(arguments(args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn only_the_graphics_probe_plan_adds_a_device() {
        let request = crate::tests::request();
        let production = JobVmConfig::new(request.clone()).unwrap();
        let control = Mode::parse("control")
            .unwrap()
            .plan(request.clone())
            .unwrap();
        let graphics = Mode::parse("graphics")
            .unwrap()
            .plan(request.clone())
            .unwrap();
        assert!(Mode::parse("virtio").is_err());
        assert_eq!(production.graphics_devices(), 1);
        assert_eq!(control.graphics_devices(), 0);
        assert_eq!(graphics.graphics_devices(), 1);
        for plan in [control, graphics] {
            assert_eq!(plan.network_devices(), 0);
            assert_eq!(plan.cpu_count(), production.cpu_count());
            assert_eq!(plan.memory_bytes(), production.memory_bytes());
            assert_eq!(plan.disks(), production.disks());
            assert_eq!(plan.shares(), production.shares());
        }
        let mut serialized = serde_json::to_value(request).unwrap();
        serialized["probe_without_graphics"] = true.into();
        assert!(serde_json::from_value::<JobVmRequest>(serialized).is_err());
    }

    #[test]
    fn metal_probe_refuses_work_inside_sealed_image_or_runner_state() {
        use std::fs;

        let root = cortexkit_test_support::scratch_root()
            .join(format!("metal-probe-paths-{}", std::process::id()));
        let image = root.join("sealed");
        let state = root.join("state");
        for directory in [&image, &state.join("targets"), &state.join("tools")] {
            fs::create_dir_all(directory).unwrap();
        }
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&state, &alias).unwrap();
        for forbidden in [image.join("work"), alias.join("tools/work")] {
            assert!(work_directory(&image, &forbidden).is_err());
            assert!(!forbidden.exists());
        }
        let work = root.join("new work");
        assert!(work_directory(&image, &work).unwrap().is_dir());
        assert!(work_directory(&image, &work).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(target_os = "macos")]
pub use host::run;

#[cfg(target_os = "macos")]
mod host {
    use super::*;
    use crate::probe::{Run, converse, invalid, prepare_job, random_hex};
    use crate::{BuildConfig, HelperReport, ProcessIdentity, process_identity};
    use motor_winagent::Frame;
    use std::fs::{self, File};
    use std::io::BufRead;
    use std::os::fd::AsFd;
    use std::time::Duration;

    pub fn run(args: &[&str]) -> io::Result<()> {
        let (image, work) = arguments(args)?;
        if unsafe { libc::geteuid() } == 0 {
            return Err(invalid(
                "run the probe as the image-building account, not root",
            ));
        }
        let image = fs::canonicalize(image)?;
        let work = work_directory(&image, work)?;
        let helper = std::env::current_exe()?;
        let source = include_str!("metal.swift").replace('\'', "'\\''");
        let command = format!("/usr/bin/swift -e '{source}'");
        let cases = [Mode::Graphics, Mode::Control].map(|mode| {
            println!("-- {} VM --", mode.name());
            let mut observation = Observation::default();
            let result = boot(&helper, &image, &work, mode, &command, &mut observation);
            (mode, observation, result)
        });
        println!("summary:");
        for (mode, observation, result) in &cases {
            let outcome = match result {
                Ok(()) => observation.outcome(),
                Err(error) => format!("incomplete: {error}"),
            };
            let timing = observation
                .hello_us
                .map_or("unavailable".into(), |us| format!("{us} us"));
            let memory = observation
                .footprint
                .map_or("unavailable".into(), |bytes| format!("{bytes} bytes"));
            println!(
                "{}: {outcome}; start to hello: {timing}; phys_footprint: {memory}",
                mode.name()
            );
        }
        for (_, observation, result) in cases {
            result?;
            if observation.guest.terminal != Some(Frame::Exit { code: 0 }) {
                return Err(invalid("the Metal command did not complete successfully"));
            }
        }
        Ok(())
    }

    #[derive(Default)]
    struct Observation {
        guest: Run,
        hello_us: Option<u128>,
        footprint: Option<u64>,
    }

    impl Observation {
        fn outcome(&self) -> String {
            if self.guest.terminal != Some(Frame::Exit { code: 0 }) {
                return format!("command ended with {:?}", self.guest.terminal);
            }
            self.guest
                .stdout
                .lines()
                .filter_map(|line| line.strip_prefix("metal: "))
                .collect::<Vec<_>>()
                .join("; ")
        }
    }

    fn boot(
        helper: &Path,
        image: &Path,
        work: &Path,
        mode: Mode,
        command: &str,
        observation: &mut Observation,
    ) -> io::Result<()> {
        let name = mode.name();
        let config = prepare_job(image, work, name)?;
        let request = config.request();
        let nonce = random_hex(16)?;
        crate::create_job_nonce_disk(
            &request.nonce_disk,
            &nonce,
            &config.mounts(),
            &BuildConfig::default(),
        )?;
        let slot = File::create(work.join(format!("{name}.slot")))?;
        let mut job = crate::spawn_helper(helper, &["metal-probe-job", name], &config, &slot)?;
        let mut printer = None;
        let measurement = (|| -> io::Result<()> {
            let reports = File::from(job.reports.as_fd().try_clone_to_owned()?);
            let (tx, rx) = std::sync::mpsc::channel();
            printer = Some(std::thread::spawn(move || {
                for line in io::BufReader::new(reports).lines().map_while(Result::ok) {
                    println!("helper: {line}");
                    match serde_json::from_str(&line) {
                        Ok(HelperReport::Started { service, .. }) => {
                            let _ = tx.send(Ok(service));
                        }
                        Ok(HelperReport::Failed { detail }) => {
                            let _ = tx.send(Err(detail));
                        }
                        _ => {}
                    }
                }
            }));
            job.open_gate()?;
            converse(
                &mut job.session,
                &nonce,
                &request.clone,
                command,
                &mut observation.guest,
                false,
            )?;
            let sampled = rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| invalid(format!("no Virtualization service report: {error}")))
                .and_then(|report| report.map_err(invalid))
                .and_then(phys_footprint);
            observation.hello_us = Some(
                fs::read_to_string(timing_path(&request.clone))?
                    .trim()
                    .parse()
                    .map_err(|error| invalid(format!("invalid helper boot timing: {error}")))?,
            );
            observation.footprint = Some(sampled?);
            Ok(())
        })();
        // Sample before closing the gate: EOF asks the helper to stop the VM.
        job.close_gate();
        let status = job.child.wait()?;
        if let Some(printer) = printer {
            let _ = printer.join();
        }
        for line in observation.guest.stdout.lines() {
            if line.starts_with("metal: ") {
                println!("{line}");
            } else {
                println!("stdout: {line}");
            }
        }
        measurement?;
        if !status.success() {
            return Err(invalid(format!("helper exited with {status}")));
        }
        Ok(())
    }

    fn phys_footprint(service: ProcessIdentity) -> io::Result<u64> {
        let sample = || -> io::Result<u64> {
            let check_identity = || {
                if process_identity(service.pid)? != service {
                    return Err(invalid("Virtualization service PID was reused"));
                }
                Ok(())
            };
            check_identity()?;
            let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
            // RUSAGE_INFO_V0 is the Darwin ABI for rusage_info_v0, including
            // ri_phys_footprint. libproc allows the image builder to read its own VM.
            if unsafe {
                libc::proc_pid_rusage(service.pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast())
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            check_identity()?;
            Ok(unsafe { usage.assume_init() }.ri_phys_footprint)
        };
        sample().map_err(|error| {
            invalid(format!(
                "cannot sample Virtualization service {} phys_footprint: {error}",
                service.pid
            ))
        })
    }
}
