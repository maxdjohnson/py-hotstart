use anyhow::{anyhow, Context, Result};
use clap::{Arg, ArgAction, Command};
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::env;

use crate::hsclient::client::{get_exit_code, initialize, take_interpreter, ensure_server};
use crate::hsclient::proxy::do_proxy;

enum Args {
    Init(String),
    Run(RunMode),
}

enum RunMode {
    Code(String),
    Module(String, Vec<String>),
    Script(String, Vec<String>),
    Repl,
}

fn parse_args() -> Result<Args> {
    let matches = Command::new("py-hotstart")
        .arg(
            Arg::new("initialize")
                .short('i')
                .long("initialize")
                .value_name("PRELUDE")
                .help("Initialize with a prelude script"),
        )
        .arg(
            Arg::new("code")
                .short('c')
                .value_name("CODE")
                .help("Program passed in as string"),
        )
        .arg(
            Arg::new("module")
                .short('m')
                .value_name("MODULE")
                .help("Run library module as a script"),
        )
        .arg(Arg::new("script").index(1).help("Script file to run"))
        .arg(
            Arg::new("script_args")
                .index(2)
                .num_args(1..) // Accept any number of additional arguments
                .action(ArgAction::Append)
                .help("Arguments passed to the script/module"),
        )
        .disable_help_flag(true)
        .disable_version_flag(true)
        .after_help("Usage: py-hotstart [options] [-c cmd | -m module | script.py] [args]")
        .get_matches();

    let prelude = matches
        .get_one::<String>("initialize")
        .map(|s| s.to_string());
    if let Some(code) = prelude {
        return Ok(Args::Init(code));
    }

    let code_mode = matches.get_one::<String>("code");
    let module_mode = matches.get_one::<String>("module");
    let script = matches.get_one::<String>("script");

    let mut script_args: Vec<String> = matches
        .get_many::<String>("script_args")
        .map(|vals| vals.cloned().collect())
        .unwrap_or_default();

    let run_mode = if let Some(c) = code_mode {
        RunMode::Code(c.to_string())
    } else if let Some(m) = module_mode {
        if let Some(s) = script {
            script_args.insert(0, s.to_string());
        }
        RunMode::Module(m.to_string(), script_args)
    } else if let Some(s) = script {
        RunMode::Script(s.to_string(), script_args)
    } else {
        RunMode::Repl
    };

    Ok(Args::Run(run_mode))
}

fn generate_instructions(run_mode: RunMode) -> Result<String> {
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let cwd_str = cwd.to_str().ok_or_else(|| anyhow!("CWD not UTF-8"))?;
    let env_vars: HashMap<String, String> = env::vars().collect();
    let env_str = json::stringify(env_vars);
    let (argv, snippet) = match run_mode {
        RunMode::Code(snip) => (
            vec!["-c".to_string()], // matches python, via `python -c "import sys; print(sys.argv)"`
            format!("exec({:?}, {{**globals(), '__name__':'__main__'}})", snip),
        ),
        RunMode::Module(module_str, mut script_args) => {
            let snip = format!(
                "import runpy; runpy.run_module({:?}, run_name='__main__', alter_sys=True)",
                &module_str
            );
            script_args.insert(0, module_str);
            (script_args, snip)
        }
        RunMode::Script(script_path, mut script_args) => {
            let snip = format!(
                "import runpy; runpy.run_path({:?}, run_name='__main__')",
                &script_path
            );
            script_args.insert(0, script_path);
            (script_args, snip)
        }
        RunMode::Repl => (
            vec!["".to_string()],
            "import code; code.interact(local={}, exitmsg='')".to_string(),
        ),
    };
    let argv_str = json::stringify(argv);

    let instructions = format!(
        r#"import sys, os

os.environ.clear()
os.environ.update({env_str})
os.chdir({cwd_str:?})
sys.argv.clear()
sys.argv.extend({argv_str})

{snippet}
"#,
    );
    Ok(instructions)
}

pub fn main() -> Result<i32> {
    ensure_server()?;
    let args = parse_args()?;
    match args {
        Args::Init(prelude_script) => {
            initialize(&prelude_script)?;
            Ok(0)
        }
        Args::Run(run_mode) => {
            let instructions = generate_instructions(run_mode)?;
            let interpreter = take_interpreter()?;
            do_proxy(interpreter.pty_master_fd.as_fd(), &instructions)?;
            let exit_code = get_exit_code(&interpreter)?;
            Ok(exit_code)
        }
    }
}
