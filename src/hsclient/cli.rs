use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::os::fd::AsFd;
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, CmsgSpace};
use nix::sys::uio::IoVec;
use nix::sys::termios::{tcgetattr, tcsetattr, Termios, LocalFlags, InputFlags, OutputFlags, ControlFlags, SetArg};
use nix::sys::signal::SIGWINCH;
use nix::sys::wait::WaitStatus;
use nix::unistd::{fork, ForkResult, close};
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use std::{env, process, fs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::hsclient::proxy::do_proxy;
use crate::hsclient::client::{initialize, take_interpreter, get_exit_code};

fn main() -> Result<()> {
    let matches = Command::new("py-hotstart")
        .arg(Arg::new("initialize")
             .short('i')
             .long("initialize")
             .takes_value(true)
             .help("Initialize with a prelude script"))
        .arg(Arg::new("code")
             .short('c')
             .takes_value(true)
             .help("Program passed in as string"))
        .arg(Arg::new("module")
             .short('m')
             .takes_value(true)
             .help("Run library module as a script"))
        .arg(Arg::new("script")
             .index(1)
             .help("Script file to run"))
        .arg(Arg::new("script_args")
             .index(2)
             .multiple_occurrences(true)
             .help("Arguments passed to the script/module"))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .after_help("Usage: py-hotstart [options] [-c cmd | -m module | script.py] [args]")
        .get_matches();

    if let Some(prelude) = matches.value_of("initialize") {
        // Initialize prelude and exit
        initialize(prelude)?;
        return Ok(());
    }

    let code_mode = matches.value_of("code");
    let module_mode = matches.value_of("module");
    let script = matches.value_of("script");
    let script_args: Vec<String> = matches.values_of("script_args")
        .map(|vals| vals.map(|v| v.to_string()).collect())
        .unwrap_or_default();

    let (exec_mode, user_code): (String, String) = if let Some(c) = code_mode {
        ("code".to_string(), c.to_string())
    } else if let Some(m) = module_mode {
        ("module".to_string(), m.to_string())
    } else if let Some(s) = script {
        ("script".to_string(), s.to_string())
    } else {
        eprintln!("No code, module, or script provided");
        process::exit(1);
    }

    let interpreter = take_interpreter()?;

    do_proxy(interpreter.pty_master_fd.as_fd(), &exec_mode, &user_code, &script_args)?;

    // Get exit code
    let exit_code = get_exit_code(&interpreter)?;
    process::exit(exit_code);
}

// Helper to get stdin fd again after main loop (since we overwrote it)
fn stdin_fd() -> RawFd {
    std::io::stdin().as_raw_fd()
}
