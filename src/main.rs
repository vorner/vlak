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

use std::env;
use std::fs::File;
use std::process;

use simplelog::{Config as LogConfig, LogLevelFilter, WriteLogger};
use tokio_core::reactor::Core;

use account::Account;
use error::{ErrorKind, Result};

fn core_loop() -> Result<!> {
    let f = File::create("log.txt")?;
    WriteLogger::init(LogLevelFilter::Debug, LogConfig::default(), f)?;
    let mut core = Core::new()?;
    let handle = core.handle();
    let token = env::args()
        .nth(1)
        .ok_or(ErrorKind::MissingToken)?;
    let account = Account::new(handle, token)?;
    core.run(account.keep_running())?;
}

fn main() {
    let Err(e) = core_loop();
    eprintln!("{}", e);
    process::exit(1);
}
