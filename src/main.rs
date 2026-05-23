use grove::migrate;

fn main() {
    let config_dir = directories::BaseDirs::new()
        .map(|b| b.config_dir().join("grove"))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config/grove"));

    if let Err(e) = migrate::run_if_needed(&config_dir) {
        eprintln!("grove: migration error: {e}");
        std::process::exit(1);
    }

    println!("grove");
}
