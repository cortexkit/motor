//! motor-runner integration tests, folded into one test binary.
mod backend_cli;
mod connect;
mod javascript;
mod logging;
#[path = "../support/process.rs"]
mod process;
mod reader_environment;
