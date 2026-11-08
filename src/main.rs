fn main() {
    let parsed = match gone::cli::parse_from(std::env::args_os()) {
        Ok(p) => p,
        Err(e) => e.exit(),
    };
    match gone::commands::main(parsed.cli, parsed.steps) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("gone: {e:#}");
            std::process::exit(2);
        }
    }
}
