use anyhow::{anyhow, Context, Result};
use clap::{Arg, ArgAction, Command};
use std::os::fd::AsFd;
use std::{env, fs, process};

use crate::hsclient::client::{get_exit_code, initialize, take_interpreter};
use crate::hsclient::proxy::do_proxy;

enum ExecMode {
    Code(String),
    Module(String),
    Script(String),
}

struct AppConfig {
    exec_mode: ExecMode,
    script_args: Vec<String>,
}

fn parse_args() -> Result<(Option<String>, AppConfig)> {
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

    let code_mode = matches.get_one::<String>("code");
    let module_mode = matches.get_one::<String>("module");
    let script = matches.get_one::<String>("script");

    let script_args: Vec<String> = matches
        .get_many::<String>("script_args")
        .map(|vals| vals.cloned().collect())
        .unwrap_or_default();

    let exec_mode = if let Some(c) = code_mode {
        ExecMode::Code(c.to_string())
    } else if let Some(m) = module_mode {
        ExecMode::Module(m.to_string())
    } else if let Some(s) = script {
        ExecMode::Script(s.to_string())
    } else {
        eprintln!("No code, module, or script provided");
        process::exit(1);
    };

    Ok((
        prelude,
        AppConfig {
            exec_mode,
            script_args,
        },
    ))
}

fn generate_env_lines() -> String {
    let mut env_lines = String::new();
    for (k, v) in env::vars() {
        let k_esc = k.replace("'", "\\'");
        let v_esc = v.replace("'", "\\'");
        env_lines.push_str(&format!("    os.environ['{k_esc}'] = '{v_esc}'\n"));
    }
    env_lines
}

fn generate_final_code(exec_mode: &ExecMode, script_args: &[String]) -> Result<String> {
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let cwd_str = cwd.to_str().ok_or_else(|| anyhow!("CWD not UTF-8"))?;

    let (main_arg, run_contents) = match exec_mode {
        ExecMode::Code(code_str) => (
            vec!["".to_string()],
            format!("exec({:?}, {{'__name__':'__main__'}})", code_str),
        ),
        ExecMode::Module(module_str) => {
            let mut argv = vec![module_str.to_string()];
            argv.extend(script_args.iter().cloned());
            (
                argv,
                format!("runpy.run_module({:?}, run_name='__main__')", module_str),
            )
        }
        ExecMode::Script(script_path) => {
            let mut argv = vec![script_path.to_string()];
            argv.extend(script_args.iter().cloned());
            let script_contents = fs::read_to_string(script_path)
                .with_context(|| format!("Failed to read script '{}'", script_path))?;
            (
                argv,
                format!("exec({:?}, {{'__name__':'__main__'}})", script_contents),
            )
        }
    };

    let argv_python_list = {
        let mut s = String::from("[");
        for arg in &main_arg {
            let a_esc = arg.replace("'", "\\'");
            s.push_str(&format!("'{}', ", a_esc));
        }
        s.push(']');
        s
    };

    let env_lines = generate_env_lines();

    let setup_code = format!(
        r#"
import sys, os, runpy

os.environ.clear()
{env_lines}
os.chdir('{cwd_str}')
sys.argv = {argv_python_list}
"#,
        env_lines = env_lines,
        cwd_str = cwd_str,
        argv_python_list = argv_python_list
    );

    Ok(format!("{setup_code}\n{run_contents}"))
}

pub fn main() -> Result<i32> {
    let (prelude, config) = parse_args()?;

    if let Some(prelude_script) = prelude {
        initialize(&prelude_script)?;
        return Ok(0);
    }

    let final_code = generate_final_code(&config.exec_mode, &config.script_args)?;
    eprintln!("{}", final_code);
    process::exit(1);

    let interpreter = take_interpreter()?;
    do_proxy(interpreter.pty_master_fd.as_fd(), &final_code)?;

    let exit_code = get_exit_code(&interpreter)?;
    Ok(exit_code)
}
