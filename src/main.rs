#![feature(clippy, never_type, conservative_impl_trait, generators, plugin, proc_macro)]
#![recursion_limit = "256"]

#[macro_use]
extern crate error_chain;
extern crate futures_await as futures;
extern crate hyper;
extern crate hyper_tls;
#[macro_use]
extern crate log;
extern crate native_tls;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde_urlencoded;
extern crate simplelog;
extern crate term;
extern crate tokio_core;
extern crate websocket;

mod account;
mod error;
mod ui;

use std::env;
use std::fs::File;
use std::process;

use futures::Future;
use simplelog::{Config as LogConfig, LogLevelFilter, WriteLogger};
use tokio_core::reactor::Core;

use account::Account;
use error::{ErrorKind, Result};
use ui::Ui;

fn core_loop() -> Result<!> {
    let f = File::create("log.txt")?;
    WriteLogger::init(LogLevelFilter::Debug, LogConfig::default(), f)?;
    let mut core = Core::new()?;
    let handle = core.handle();
    let token = env::args()
        .nth(1)
        .ok_or(ErrorKind::MissingToken)?;
    let account = Account::new(handle, token)?;
    let ui = Ui::new(account.events());
    let ac_run = account.keep_running();
    let ui_run = ui.run();
    let run_all = ac_run.select(ui_run).map(|(nothing, _)| nothing).map_err(|(e, _)| e);
    core.run(run_all)?
}

fn main() {
    let Err(e) = core_loop();
    eprintln!("{}", e);
    process::exit(1);
}
