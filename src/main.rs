use std::io;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
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
