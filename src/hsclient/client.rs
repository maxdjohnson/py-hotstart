use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, CmsgSpace};
use nix::sys::uio::IoVec;
use std::env;
use std::process;

const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";

fn send_request(req: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(SOCKET_PATH)
        .context("Failed to connect to server")?;
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    Ok(stream)
}

fn initialize_prelude(prelude: &str) -> Result<()> {
    let mut stream = send_request(&format!("INIT {}", prelude))?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).to_string();
    if resp.trim() == "OK" {
        Ok(())
    } else {
        anyhow::bail!("INIT failed: {}", resp)
    }
}

fn request_run() -> Result<RawFd> {
    let mut stream = send_request("RUN")?;
    let mut iov = [IoVec::from_mut_slice(&mut [0u8; 2])];
    let mut cmsgspace = CmsgSpace::<[RawFd; 1]>::new();

    let msg = recvmsg(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgspace),
        MsgFlags::empty(),
    ).context("Failed to recvmsg")?;

    if msg.bytes == 0 {
        anyhow::bail!("No data received from server");
    }
    let resp = &iov[0];
    let resp_str = String::from_utf8_lossy(resp);

    // Extract FD from cmsg
    let mut pty_fd: Option<RawFd> = None;
    for cmsg in msg.cmsgs() {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            pty_fd = fds.get(0).copied();
        }
    }

    if resp_str.contains("OK") && pty_fd.is_some() {
        Ok(pty_fd.unwrap())
    } else {
        anyhow::bail!("RUN request failed or no FD received")
    }
}

fn request_exitcode() -> Result<i32> {
    let mut stream = send_request("EXITCODE")?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    resp.parse::<i32>().context("Failed to parse exit code from server")
}

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
             .help("Run library module as a script, e.g. 'py-hotstart -m http.server'"))
        .arg(Arg::new("script")
             .index(1)
             .help("Script file to run"))
        .arg(Arg::new("script_args")
             .index(2)
             .multiple_occurrences(true)
             .help("Arguments passed to the script or module"))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .after_help("Similar usage to python: py-hotstart [options] [-c cmd | -m module | script] [args]")
        .get_matches();

    if let Some(prelude) = matches.value_of("initialize") {
        // Initialize prelude and exit
        initialize_prelude(prelude)?;
        return Ok(());
    }

    let code_mode = matches.value_of("code");
    let module_mode = matches.value_of("module");
    let script = matches.value_of("script");
    let script_args: Vec<String> = matches.values_of("script_args").map(|vals| vals.map(|v| v.to_string()).collect()).unwrap_or_default();

    // Determine execution mode
    let (exec_mode, user_code): (String, String) = if let Some(c) = code_mode {
        // `-c "print('hello')"`
        ("code".to_string(), c.to_string())
    } else if let Some(m) = module_mode {
        // `-m module`
        ("module".to_string(), m.to_string())
    } else if let Some(s) = script {
        // `script.py [args]`
        ("script".to_string(), s.to_string())
    } else {
        // No code given, act like python interactive? Or error out
        eprintln!("No code, module, or script provided");
        process::exit(1);
    }

    let pty_fd = request_run()?;

    // Build the environment setup code
    // Get current working directory
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let cwd_str = cwd.to_str().ok_or_else(|| anyhow::anyhow!("CWD is not valid UTF-8"))?;

    // Build environment dict from env::vars()
    let env_vars: Vec<(String, String)> = env::vars().collect();
    // Construct Python dict of environ
    let mut env_lines = String::new();
    for (k,v) in env_vars {
        // escape quotes
        let k_escaped = k.replace("'", "\\'");
        let v_escaped = v.replace("'", "\\'");
        env_lines.push_str(&format!("    os.environ['{k_escaped}'] = '{v_escaped}'\n"));
    }

    // Build sys.argv
    // For code (-c), python sets sys.argv = [''] by convention.
    // For module (-m mod), sys.argv = [mod] + script_args.
    // For script, sys.argv = [script] + script_args.
    let mut argv = vec![];
    match exec_mode.as_str() {
        "code" => {
            // sys.argv = ['']
            argv.push("".to_string());
            // We will exec user_code directly
        }
        "module" => {
            argv.push(user_code.clone());
            argv.extend(script_args.iter().cloned());
        }
        "script" => {
            argv.push(user_code.clone());
            argv.extend(script_args.iter().cloned());
        }
        _ => {}
    }

    let argv_python_list = {
        let mut s = String::from("[");
        for arg in argv.iter() {
            let a_esc = arg.replace("'", "\\'");
            s.push_str(&format!("'{}', ", a_esc));
        }
        s.push(']');
        s
    };

    // Prepend code that sets environment, cwd, and argv
    let setup_code = format!(r#"
import sys, os, runpy

# Set environment
os.environ.clear()
{env_lines}

# Set cwd
os.chdir('{cwd_str}')

# Set argv
sys.argv = {argv_python_list}

"#, env_lines=env_lines, cwd_str=cwd_str, argv_python_list=argv_python_list);

    // Now append code to execute user’s request
    // For -c code: just exec the code read from user_code
    // For -m module: run `runpy.run_module(module, run_name='__main__')`
    // For script: exec the script file contents.
    let final_code = match exec_mode.as_str() {
        "code" => {
            // Just exec the user_code
            format!("{setup_code}\nexec({:?}, {{'__name__':'__main__'}})", user_code)
        },
        "module" => {
            // run module
            format!("{setup_code}\nrunpy.run_module({:?}, run_name='__main__')", user_code)
        },
        "script" => {
            // read file and exec
            // The interpreter is expecting code from stdin.
            // We'll have the client read the file and send its contents as code.
            let script_contents = std::fs::read_to_string(&user_code)
                .with_context(|| format!("Failed to read script '{}'", user_code))?;
            format!("{setup_code}\nexec({:?}, {{'__name__':'__main__'}})", script_contents)
        },
        _ => unreachable!()
    };

    // Set up PTY forwarding
    // In a production setup, we would:
    // - Put terminal in raw mode
    // - Spawn a thread or fork to copy input from stdin to pty, and output from pty to stdout
    // - Forward signals
    // For simplicity, we’ll just do blocking I/O. This won't handle signals gracefully.
    unsafe {
        libc::fcntl(pty_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    // Fork a child to handle I/O bridging
    match unsafe { nix::unistd::fork()? } {
        nix::unistd::ForkResult::Parent { child: _ } => {
            // In parent: write code to interpreter pty
            use std::os::unix::io::FromRawFd;
            let mut pty_master = unsafe { std::fs::File::from_raw_fd(pty_fd) };
            pty_master.write_all(final_code.as_bytes())?;
            pty_master.flush()?;
            // Close write side so the interpreter knows no more input
            drop(pty_master);

            // Wait for interpreter to exit by reading from server exit code
            // In a more complete implementation, the parent would wait on the child's I/O forwarding.
            let exit_code = request_exitcode()?;
            process::exit(exit_code);
        }
        nix::unistd::ForkResult::Child => {
            // In child: forward IO from pty to stdout and from stdin to pty
            // This is a simplistic forwarding loop
            use std::os::fd::FromRawFd;
            let mut pty_master = unsafe { std::fs::File::from_raw_fd(pty_fd) };

            // Non-blocking or raw mode handling would be needed in real code.
            let mut buf = [0u8; 1024];
            loop {
                let n = pty_master.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                std::io::stdout().write_all(&buf[..n])?;
                std::io::stdout().flush()?;
            }
            process::exit(0);
        }
    }
}
