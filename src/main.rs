fn main() {
    if let Err(error) = keiros::run() {
        eprintln!("{}", keiros::ui::Ui::stderr().error(format!("{error:#}")));
        std::process::exit(1);
    }
}
