use motor_module::{
    module::{manifest_with_config, Handler},
    Config,
};
use std::{io, path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if version_requested(&args) {
        println!("{}", binary_version());
        return Ok(());
    }
    // `ck motor ...` runs this binary as a command-line tool. The daemon
    // starts the module with `--subc FILE`, never with these first words, so
    // the module start below is unchanged.
    match args.first().map(String::as_str) {
        Some("--ck-domain") if args.len() == 1 => {
            println!("{}", motor_module::cli::HEADLINE);
            return Ok(());
        }
        Some("run") => std::process::exit(motor_module::cli::main(&args[1..]).await.into()),
        // `ck motor`, `ck motor --help` and `ck motor help` all land here; the
        // daemon always passes --subc, so none of these can be a module start.
        None | Some("--help" | "-h" | "help") if args.len() <= 1 => {
            println!("{}", motor_module::cli::TOP_HELP);
            return Ok(());
        }
        _ => {}
    }
    // Consume the daemon nonce pipe before spawning git or SSH children.
    let _ = subc_os::launch_nonce()?;
    // Without a subscriber every tracing event (including the unreachable
    // refusal diagnostics) is silently discarded; route them to stderr, which
    // the daemon captures as this module's log.
    if let Err(error) = motor_module::logging::init() {
        eprintln!("[ck-motor] logging unavailable: {error}");
    }
    let mut args = args.into_iter();
    let mut connection = None;
    let mut config = std::env::var_os("CK_MOTOR_CONFIG").map(PathBuf::from);
    let mut print = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--subc" => {
                connection = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| io::Error::other("--subc needs a path"))?,
                ))
            }
            "--config" => {
                config = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| io::Error::other("--config needs a path"))?,
                ))
            }
            "--print-manifest" => print = true,
            _ => return Err(io::Error::other(format!("unknown option {arg}")).into()),
        }
    }
    let id = std::env::var("SUBC_MODULE_ID").unwrap_or_else(|_| "ck-motor".into());
    let config = match config {
        Some(path) => Config::load(&path)?,
        None => Config::default(),
    };
    let manifest = manifest_with_config(&id, &config);
    if print {
        println!("{}", serde_json::to_string(&manifest)?);
        return Ok(());
    }
    let connection = connection
        .ok_or_else(|| io::Error::other("usage: ck-motor --subc FILE [--config FILE]"))?;
    let resolver = Arc::new(
        cortexkit_claustrum_client::ClaustrumCredentialResolver::new(
            connection.clone(),
            std::env::current_dir()?,
            id.clone(),
        ),
    );
    let handler = Handler::new(config).with_credentials(resolver);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let result = tokio::select! {
        result = async {
            let (_, serving) = subc_client_rs::serve_with_handle(&connection, manifest, handler.clone()).await?;
            serving.await
        } => result,
        _ = terminate.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
    };
    // The SDK's end hook covers normal GOODBYE/EOF; this also covers wire errors.
    handler.shutdown().await;
    result?;
    Ok(())
}

fn version_requested(args: &[String]) -> bool {
    args.len() == 1 && args[0] == "--version"
}

fn binary_version() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")
}

#[cfg(test)]
mod tests {
    use super::{binary_version, version_requested};

    #[test]
    fn version_argument_is_recognized_without_daemon_arguments() {
        assert!(version_requested(&["--version".into()]));
        assert!(!version_requested(&[]));
        assert!(!version_requested(&["--version".into(), "extra".into()]));
        assert!(binary_version().starts_with(concat!(env!("CARGO_PKG_VERSION"), " (")));
        assert!(binary_version().ends_with(')'));
    }
}
