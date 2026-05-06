fn main() {
    if let Err(err) = pug::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
