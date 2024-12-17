mod hsclient;
mod hsserver;

fn main() {
    match hsclient::cli::main() {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("Error occurred: {}", e);
            std::process::exit(1); // Exit with a non-zero code on error
        }
    }
}
