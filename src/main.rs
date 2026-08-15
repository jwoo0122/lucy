use std::io;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 2 && args[0] == "gateway" && args[1] == "telegram" {
        if let Err(error) = lucy::gateway::telegram::run() {
            eprintln!("!: {error}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "gateway") {
        eprintln!("!: usage: lucy gateway telegram");
        std::process::exit(2);
    }
    if args.first().is_some_and(|arg| arg == "history") {
        let exit_code =
            lucy::history::run_cli(&args[1..], io::stdout().lock(), io::stderr().lock());
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return;
    }

    let exit_code = lucy::run_cli(
        &args,
        io::BufReader::new(io::stdin()),
        io::stdout().lock(),
        io::stderr().lock(),
    );
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}
