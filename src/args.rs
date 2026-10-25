//! Command-line parsing for the supported subset of GNU du:
//! -h/--human-readable, -k, -d/--max-depth and -s/--summarize, with
//! getopt-style clustering (`-hd1`) and operands allowed anywhere.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub human: bool,
    pub max_depth: Option<usize>,
    pub summarize: bool,
    pub paths: Vec<Vec<u8>>,
    /// Non-fatal diagnostics to print before running.
    pub warnings: Vec<String>,
}

pub fn parse(argv: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = argv.into_iter().map(OsString::into_vec);
    while let Some(a) = it.next() {
        if a == b"--" {
            args.paths.extend(it);
            break;
        }
        if let Some(long) = a.strip_prefix(b"--") {
            parse_long(long, &mut it, &mut args)?;
        } else if a.len() > 1 && a[0] == b'-' {
            parse_short(&a[1..], &mut it, &mut args)?;
        } else {
            args.paths.push(a);
        }
    }
    if args.paths.is_empty() {
        args.paths.push(b".".to_vec());
    }
    apply_summarize(&mut args)?;
    Ok(args)
}

/// -s means --max-depth=0. GNU warns about a redundant -d0 but rejects any
/// other depth, regardless of which flag came first.
fn apply_summarize(args: &mut Args) -> Result<(), String> {
    if !args.summarize {
        return Ok(());
    }
    match args.max_depth {
        None => args.max_depth = Some(0),
        Some(0) => args.warnings.push("warning: summarizing is the same as using --max-depth=0".into()),
        Some(n) => return Err(format!("warning: summarizing conflicts with --max-depth={n}")),
    }
    Ok(())
}

// TODO: GNU getopt also accepts unambiguous abbreviations like --max.
fn parse_long(opt: &[u8], rest: &mut impl Iterator<Item = Vec<u8>>, args: &mut Args) -> Result<(), String> {
    let (name, value) = match opt.iter().position(|&c| c == b'=') {
        Some(i) => (&opt[..i], Some(opt[i + 1..].to_vec())),
        None => (opt, None),
    };
    match name {
        b"human-readable" if value.is_some() => Err("option '--human-readable' doesn't allow an argument".into()),
        b"human-readable" => {
            args.human = true;
            Ok(())
        }
        b"summarize" if value.is_some() => Err("option '--summarize' doesn't allow an argument".into()),
        b"summarize" => {
            args.summarize = true;
            Ok(())
        }
        b"max-depth" => {
            let value = value.or_else(|| rest.next()).ok_or("option '--max-depth' requires an argument")?;
            args.max_depth = Some(parse_depth(&value)?);
            Ok(())
        }
        _ => Err(format!("unrecognized option '--{}'", String::from_utf8_lossy(opt))),
    }
}

fn parse_short(opts: &[u8], rest: &mut impl Iterator<Item = Vec<u8>>, args: &mut Args) -> Result<(), String> {
    for (i, &c) in opts.iter().enumerate() {
        match c {
            b'h' => args.human = true,
            // -h and -k both pick the unit; the last one wins.
            b'k' => args.human = false,
            b's' => args.summarize = true,
            b'd' => {
                let attached = &opts[i + 1..];
                let value = match attached.is_empty() {
                    true => rest.next().ok_or("option requires an argument -- 'd'")?,
                    false => attached.to_vec(),
                };
                args.max_depth = Some(parse_depth(&value)?);
                return Ok(());
            }
            _ => return Err(format!("invalid option -- '{}'", c as char)),
        }
    }
    Ok(())
}

fn parse_depth(value: &[u8]) -> Result<usize, String> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("invalid maximum depth '{}'", String::from_utf8_lossy(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(argv: &[&str]) -> Result<Args, String> {
        parse(argv.iter().map(OsString::from))
    }

    fn want(human: bool, max_depth: Option<usize>, paths: &[&str]) -> Args {
        let paths = paths.iter().map(|p| p.as_bytes().to_vec()).collect();
        Args { human, max_depth, paths, ..Args::default() }
    }

    fn summarized(human: bool, paths: &[&str]) -> Args {
        Args { summarize: true, ..want(human, Some(0), paths) }
    }

    #[test]
    fn parses_valid_args() {
        let cases: &[(&str, &[&str], Args)] = &[
            ("defaults to dot", &[], want(false, None, &["."])),
            ("human short", &["-h"], want(true, None, &["."])),
            ("human long", &["--human-readable", "x"], want(true, None, &["x"])),
            ("depth separate", &["-d", "2"], want(false, Some(2), &["."])),
            ("depth attached", &["-d0"], want(false, Some(0), &["."])),
            ("clustered", &["-hd1", "a"], want(true, Some(1), &["a"])),
            ("clustered separate value", &["-hd", "3"], want(true, Some(3), &["."])),
            ("long with equals", &["--max-depth=4"], want(false, Some(4), &["."])),
            ("long with separate value", &["--max-depth", "5"], want(false, Some(5), &["."])),
            ("options after operands", &["a", "-h", "b"], want(true, None, &["a", "b"])),
            ("double dash ends options", &["--", "-h"], want(false, None, &["-h"])),
            ("lone dash is an operand", &["-"], want(false, None, &["-"])),
            ("last depth wins", &["-d1", "-d2"], want(false, Some(2), &["."])),
            ("summarize short", &["-s", "a"], summarized(false, &["a"])),
            ("summarize long", &["--summarize"], summarized(false, &["."])),
            ("summarize human clustered", &["-sh"], summarized(true, &["."])),
            ("kilobytes is the default unit", &["-sk"], summarized(false, &["."])),
            ("k after h wins", &["-h", "-k"], want(false, None, &["."])),
            ("h after k wins", &["-kh"], want(true, None, &["."])),
        ];
        for (name, argv, expected) in cases {
            assert_eq!(parse_strs(argv).as_ref(), Ok(expected), "{name}");
        }
    }

    #[test]
    fn rejects_invalid_args() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("unknown short", &["-x"], "invalid option -- 'x'"),
            ("unknown long", &["--apparent-size"], "unrecognized option '--apparent-size'"),
            ("missing depth", &["-d"], "option requires an argument -- 'd'"),
            ("missing long depth", &["--max-depth"], "option '--max-depth' requires an argument"),
            ("negative depth", &["-d", "-1"], "invalid maximum depth '-1'"),
            ("non-numeric depth", &["--max-depth=x"], "invalid maximum depth 'x'"),
            ("human with value", &["--human-readable=1"], "option '--human-readable' doesn't allow an argument"),
            ("summarize with depth", &["-s", "-d1"], "warning: summarizing conflicts with --max-depth=1"),
            ("depth before summarize", &["-d2", "-s"], "warning: summarizing conflicts with --max-depth=2"),
        ];
        for (name, argv, msg) in cases {
            assert_eq!(parse_strs(argv), Err(msg.to_string()), "{name}");
        }
    }

    #[test]
    fn warns_on_redundant_depth() {
        let args = parse_strs(&["-s", "-d0"]).unwrap();
        assert_eq!(args.max_depth, Some(0));
        assert_eq!(args.warnings, ["warning: summarizing is the same as using --max-depth=0"]);
    }
}
