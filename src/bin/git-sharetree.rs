//! `git sharetree ...`: git runs `git-sharetree` from PATH for unknown subcommands.

fn main() {
    std::process::exit(sharetree::cli::run());
}
