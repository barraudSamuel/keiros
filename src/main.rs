fn main() {
    if let Err(error) = kairos::run() {
        eprintln!("{}", kairos::ui::Ui::stderr().error(format!("{error:#}")));
        std::process::exit(1);
    }
}
