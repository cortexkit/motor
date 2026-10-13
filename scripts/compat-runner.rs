//! Wire-compatibility helper using the oldest supported runner from scripts/compat-client-revision.
use std::{io, path::PathBuf};
use tokio::{io::AsyncWriteExt, net::UnixListener};

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    assert_eq!(args.len(), 3, "state-root socket workspace-root");
    let mut config = motor_runner::Config::new(PathBuf::from(&args[0]), "compat");
    config.isolate_jobs = false;
    config.free_space_floor_bytes = 0;
    config.max_weight = 8;
    config.base_commands = vec!["true".into()];
    config.workspace_roots = vec![PathBuf::from(&args[2])];
    let runner = motor_runner::Runner::open(config)?;
    let listener = UnixListener::bind(&args[1])?;
    tokio::io::stdout().write_all(b"ready\n").await?;
    motor_runner::serve(runner, listener).await
}
