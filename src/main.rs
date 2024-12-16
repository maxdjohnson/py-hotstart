mod hsclient;
mod hsserver;
mod hstty;

use anyhow::{anyhow, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "py-hotstart",
    about = "A python CLI with instant startup via an interpreter server."
)]
struct Args {
    /// (Re)create py-hotstart server with the string included
    #[arg(short = 'i', long = "initialize", value_name = "PYTHON_CODE")]
    initialize: Option<String>,

    /// Program passed in as string.
    #[arg(short = 'c', long = "code", value_name = "PYTHON_CODE")]
    code_snippet: Option<String>,

    /// Run library module as a script.
    #[arg(short = 'm', long = "module", value_name = "MODULE")]
    module_name: Option<String>,

    /// Program read from script file.
    file: Option<String>,

    /// Arguments passed to program in sys.argv[1:].
    /// Use `--` to delimit these arguments from the main command arguments.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

impl Args {
    fn new() -> Self {
        let mut cli = Args::parse();

        // If we have a module name and a file, then file should really be part of module args
        if cli.module_name.is_some() && cli.file.is_some() {
            if let Some(file) = cli.file.take() {
                cli.args.insert(0, file);
            }
        }

        cli
    }
}

fn main() {
    let args = Args::new();
    if let Err(e) = run(args) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    if let Some(prelude_code) = args.initialize {
        return hsclient::start_server(&prelude_code);
    } else if let Some(module_name) = args.module_name {
        return hstty.run_module(module_name, args.args);
    } else if let Some(file_name) = args.file {
        return hstty.run_file(file_name, args.args);
    } else if let Some(python_code) = args.code_snippet {
        return hstty.run_code(python_code);
    } else {
        return hstty.run_repl();
    }
}
