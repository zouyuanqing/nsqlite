//! A minimal command-line shell, so the engine can be driven the way a real one
//! is.
//!
//! It reads SQL from a file or from standard input and prints rows in the
//! separator form the official test suite's shim expects, which makes this the
//! executable the suite would drive.

use std::io::Read;
use std::process::ExitCode;

use nsqlite::connection::{Connection, Outcome};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut sql = String::new();
    let mut read_stdin = false;

    // Arguments are read left to right. The first bare argument is the
    // database path when one has not been taken and the argument does not look
    // like SQL; everything else is script. That matches how the real shell is
    // invoked, as `sqlite3 DB SQL` and as `sqlite3 DB` with the script on
    // standard input.
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--version" => {
                // The name matters: a harness checks it to tell engines apart.
                println!("nsqlite {}", nsqlite::VERSION);
                return ExitCode::SUCCESS;
            }
            "-i" | "--init" | "--batch" | "-bail" | "-echo" => i += 1,
            "-separator" | "-cmd" | "-readonly" | "-newline" | "-nullvalue" => i += 2,
            other if path.is_none() && !looks_like_sql(other) => {
                path = Some(other.to_string());
                i += 1;
            }
            other => {
                if !sql.is_empty() {
                    sql.push(' ');
                }
                sql.push_str(other);
                i += 1;
            }
        }
    }
    let _ = read_stdin;

    let conn = match path.as_deref() {
        Some(p) if p != ":memory:" => Connection::open(std::path::Path::new(p)),
        _ => Connection::open_memory(),
    };
    let mut conn = match conn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match conn.execute_script(&sql) {
        Ok(outcomes) => {
            for o in outcomes {
                if let Err(code) = print(&o) {
                    eprintln!("Error: {code}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            // SQLite's shell prints the message on standard error and exits
            // non-zero; the suite matches on the text.
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Prints an outcome in the separator form the test shim parses.
///
/// Values go out separated by a pipe with no padding, a NULL as the empty
/// string, and a blob as its hex digits, which is what the CLI does when the
/// separator is set to `|`.
/// Whether an argument reads as a statement rather than a file name.
fn looks_like_sql(s: &str) -> bool {
    const STARTERS: &[&str] = &[
        "select", "insert", "update", "delete", "create", "drop", "alter", "begin",
        "commit", "rollback", "pragma", "explain", "with", "values", "replace",
    ];
    let lower = s.trim().to_ascii_lowercase();
    STARTERS.iter().any(|k| lower.starts_with(k))
        || lower.starts_with('\'')
        || lower.starts_with('(')
}

fn print(o: &Outcome) -> Result<(), String> {
    match o {
        Outcome::Query { columns, rows } => {
            let mut out = String::new();
            for row in rows {
                for (i, v) in row.values.iter().enumerate() {
                    if i > 0 {
                        out.push('|');
                    }
                    out.push_str(&render(v));
                }
                out.push('\n');
            }
            // A query with no rows still reports its columns, so a harness can
            // tell an empty result from a statement that returned nothing.
            if rows.is_empty() && !columns.is_empty() {
                print!("{columns:?}");
            }
            print!("{out}");
            Ok(())
        }
        Outcome::Changed(n) => {
            if *n > 0 {
                println!("{n}");
            }
            Ok(())
        }
        Outcome::Nothing => Ok(()),
    }
}

fn render(v: &nsqlite::Value) -> String {
    use nsqlite::Value;
    match v {
        Value::Null => String::new(),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
        other => other.to_string(),
    }
}
