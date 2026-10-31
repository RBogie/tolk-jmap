//! Turns one device's captured traffic into a sanitized fixture session.
//!
//! ```text
//! eas-sanitize <traffic dir> <device id> <output dir>
//! ```
//!
//! Review the `.xml` files before committing a session: the allowlist keeps
//! structural values only, but a device may put personal data in an
//! unexpected place.

use std::{path::PathBuf, process::ExitCode};

use tolk::fixtures;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [traffic, device, out] = args.as_slice() else {
        eprintln!("usage: eas-sanitize <traffic dir> <device id> <output dir>");
        return ExitCode::from(2);
    };
    let result = fixtures::read_capture(&PathBuf::from(traffic), device).and_then(|exchanges| {
        fixtures::write_session(&PathBuf::from(out), &exchanges)?;
        Ok(exchanges.len())
    });
    match result {
        Ok(count) => {
            println!("wrote {count} exchanges to {out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("eas-sanitize: {e:#}");
            ExitCode::FAILURE
        }
    }
}
