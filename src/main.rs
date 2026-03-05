use anyhow::{Result, bail};
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() >= 2 && args[1] == "config" {
        return run_config_command(&args[2..]);
    }

    fish_session::ui::run_ui()
}

fn run_config_command(args: &[String]) -> Result<()> {
    if args.len() == 2 && args[0] == "key" {
        let config = fish_session::config::AppConfig::load().unwrap_or_default();
        match args[1].as_str() {
            "open" => {
                println!("{}", config.open_key_binding());
                return Ok(());
            }
            "detach" => {
                println!("{}", config.detach_key_binding());
                return Ok(());
            }
            _ => {}
        }
    }

    bail!("usage: fish-session config key <open|detach>")
}
