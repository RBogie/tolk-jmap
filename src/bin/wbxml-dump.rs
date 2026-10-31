//! Prints WBXML files as XML: `wbxml-dump traffic/*.wbxml`.

use std::process::ExitCode;

use tolk::wbxml;

fn main() -> ExitCode {
    let paths: Vec<_> = std::env::args_os().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: wbxml-dump <file>...");
        return ExitCode::FAILURE;
    }
    let mut ok = true;
    for path in &paths {
        let shown = path.to_string_lossy();
        if paths.len() > 1 {
            println!("==> {shown}");
        }
        match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| wbxml::decode(&bytes).map_err(|e| e.to_string()))
        {
            Ok(root) => print!("{}", wbxml::to_xml(&root)),
            Err(e) => {
                eprintln!("{shown}: {e}");
                ok = false;
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
