#[path = "../update.rs"]
mod update;

fn main() {
    std::process::exit(update::updater_main());
}
