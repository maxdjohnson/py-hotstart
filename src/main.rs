use anyhow::{anyhow, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "py-hotstart",
    about = "A python CLI with instant startup via an interpreter server."
)]
struct Cli {
    /// Code snippet to be used
    #[arg(short = 'c', long = "code", value_name = "PYTHON_CODE")]
    code_snippet: Option<String>,

    /// Whether to start with imports
    #[arg(short = 'i', long = "imports", value_name = "PYTHON_CODE")]
    start_with_imports: Option<String>,

    /// Module name
    ///
    /// If this is specified, additional arguments after `--` will be treated as the module's arguments.
    #[arg(short = 'm', long = "module", value_name = "MODULE")]
    module_name: Option<String>,

    /// File name argument
    file_name: Option<String>,

    /// Arguments passed through to the python process.
    /// Use `--` to delimit these arguments from the main command arguments.
    #[arg(trailing_var_arg = true)]
    additional_args: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let code_snippet = cli.code_snippet.unwrap_or_default();
    let start_with_imports = cli.start_with_imports.unwrap_or_default();
    let mut additional_args = cli.additional_args;
    let module_name = cli.module_name.unwrap_or_default();
    if let Some(module_name) = cli.module_name {
        if let Some(file_name) = cli.file_name {
            let mut args_with_file = vec![file_name];
            args_with_file.extend(additional_args);
            additional_args = file_name
        }
    }
    let file_name = cli.file_name.unwrap_or_default();

    if code_snippet.is_empty() && module_name.is_empty() && file_name.is_empty() {
        return Err(anyhow!("No arguments provided."));
    }

    // Implement your main logic here
    println!("Code snippet: {}", code_snippet);
    println!("Module name: {}", module_name);
    println!("Module args: {:?}", additional_args);
    println!("File name: {}", file_name);
    println!("Start with imports: {}", start_with_imports);

    Ok(())
}
