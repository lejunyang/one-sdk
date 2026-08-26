//! Process-isolated entry point for Aube operations that intentionally mutate
//! process-wide state (notably `aube add --global`).

fn main() {
    std::process::exit(aube::cli_main(&aube::embed::AUBE));
}
