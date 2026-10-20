// A failure is built once per run and printed; its size does not matter.
#![allow(clippy::result_large_err)]

mod cli;
mod commands;
mod help;
mod model;
mod os;
mod probe;
mod report;
mod store;
mod supervisor;

use cli::Cmd;
use report::{Failure, Output};
use serde_json::json;

fn main() {
    let (json, cmd) = cli::parse(std::env::args().skip(1).collect());
    let result = match cmd {
        Ok(Cmd::Supervise) => std::process::exit(supervisor::run().into()),
        Ok(Cmd::Help(name)) => match help::render(name.as_deref()) {
            Ok(text) => Ok(Output::new(text.clone(), json!({ "help": text }))),
            Err(message) => Err(Failure::usage(message)),
        },
        Ok(Cmd::Version) => Ok(Output::new(
            format!("devsrv {}\n", env!("CARGO_PKG_VERSION")),
            json!({ "version": env!("CARGO_PKG_VERSION") }),
        )),
        Ok(Cmd::Start(args)) => commands::start(args),
        Ok(Cmd::Ls { all }) => commands::ls(all),
        Ok(Cmd::Logs {
            name,
            lines,
            follow,
            timeout,
        }) => commands::logs(name, lines, follow, timeout),
        Ok(Cmd::Stop { names, all }) => commands::stop(names, all),
        Ok(Cmd::Who { port }) => commands::who(port),
        Ok(Cmd::Wait { target, timeout }) => commands::wait(target, timeout),
        Err(f) => Err(f),
    };
    std::process::exit(report::emit(result, json).into());
}
