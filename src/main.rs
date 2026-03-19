fn main() {
    if let Err(error) = keiros::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}