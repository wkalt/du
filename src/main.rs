mod args;
mod human;
mod queue;
mod report;
mod scan;
mod sys;
mod tree;

use std::collections::HashSet;
use std::ffi::CString;
use std::io::{self, BufWriter};
use std::process::ExitCode;
use std::thread;

use args::Args;
use report::{Options, Report};
use scan::Scanner;
use tree::{ErrorKind, Root, ScanError};

fn main() -> ExitCode {
    // Die quietly on a closed pipe (`du | head`), like coreutils.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let args = match args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("du: {msg}\nTry 'du --help' for more information.");
            return ExitCode::FAILURE;
        }
    };
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("du: failed to write output: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> io::Result<bool> {
    let max_depth = args.max_depth.unwrap_or(usize::MAX);
    let dedup_all = args.paths.len() > 1;
    let stats: Vec<_> = args.paths.into_iter().map(tree::trim_operand).map(|p| (lstat(&p), p)).collect();

    // With overlapping operands, a single-link file named on the command
    // line may also be found inside a directory operand.
    let tracked: HashSet<_> = match dedup_all {
        true => stats.iter().filter_map(|(s, _)| s.as_ref().ok()).filter(|s| !s.is_dir).map(|s| s.key).collect(),
        false => HashSet::new(),
    };

    let scanner = Scanner::new(tracked, max_depth);
    let roots: Vec<Root> = stats
        .into_iter()
        .map(|(stat, path)| match stat {
            Err(e) => Root::Missing(ScanError::new(ErrorKind::Access, path, e)),
            Ok(stat) if stat.is_dir => Root::Dir(scanner.add_root(path, stat)),
            Ok(stat) => Root::File { path, stat },
        })
        .collect();

    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let tree = scanner.run(threads);

    let out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    let mut report = Report::new(&tree, Options { human: args.human, max_depth, dedup_all }, out);
    for root in &roots {
        report.root(root)?;
    }
    report.finish()
}

fn lstat(path: &[u8]) -> io::Result<sys::Stat> {
    let cpath = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    sys::lstat(&cpath)
}
