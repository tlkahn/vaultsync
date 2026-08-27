fn main() {
    let code = vaultsync::cli::run_from_env();
    std::process::exit(code);
}
