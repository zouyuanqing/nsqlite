//! A minimal command-line shell, so the engine can be driven the way a real one
//! is.
//!
//! It reads SQL from a file or from standard input and prints rows in the
//! separator form the official test suite's shim expects, which makes this the
//! executable the suite would drive.
//!
//! There is a second mode, `--testsuite`, which prints the record stream the
//! TCL shim parses. See the `testsuite` module for why the row format below is
//! not what that shim reads.

use std::process::ExitCode;

use nsqlite::connection::{Connection, Outcome};

mod testsuite;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut sql = String::new();
    let mut suite_mode = false;

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
            "--testsuite" => {
                suite_mode = true;
                i += 1;
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

    if suite_mode {
        // The record stream is the shim's only channel, so an error goes on it
        // rather than on stderr: a Tcl error is how do_test learns a statement
        // failed, and it needs the message, not a bare exit status.
        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        // The script may be on standard input rather than on the command line.
        // It has to be able to be: a test's SQL is not bounded, and the command
        // line is -- Windows caps it at 32767 characters, and createtab and
        // types both build statements past that. `nsqlited --testsuite DB` with
        // the script piped in is the form that has no limit.
        if sql.is_empty() {
            // THE WHOLE SCRIPT IS READ BEFORE ANY OF IT RUNS, and that is
            // deliberate. An earlier version of this file streamed the script a
            // line at a time, to avoid blocking on a writer that never closes
            // stdin, and it was wrong: a line is not a statement. The semicolons
            // inside a trigger body
            //
            //     CREATE TRIGGER tg ... BEGIN SELECT 1; SELECT 2; END;
            //
            // split one statement into three, so each fragment reached the
            // parser alone. `test/shim/tester.tcl` says the same thing outright:
            // the split point has to be a statement boundary, which means the
            // parser has to see the statement, which means the statement has to
            // have been read.
            //
            // MEASURED, so the reason is not folklore: a CREATE TRIGGER with a
            // BEGIN...END body is a SYNTAX ERROR in this engine today, on one
            // line and across many, and it is the PARSER that refuses it, not
            // this loop -- the 15:37 binary, built before any of this, answers
            // `E near "END": syntax error` for the same input. So the earlier
            // comment here, which blamed the line splitting for that error, was
            // wrong twice over: the splitting did not cause it, and removing
            // the splitting did not fix it. Trigger bodies are a parser gap
            // (the suite's trigger1/2/3.test hold 37, 26 and 4 of them), and
            // this loop is not where that is fixed.
            //
            // The stdin-blocking hazard is still real and still unfixed -- a
            // caller that spawns this binary with a live stdin handle and never
            // closes it hangs here, and the hung process then holds the image of
            // `nsqlited.exe` so that every `cargo build` fails with "access
            // denied". Fixing it needs a statement-boundary splitter that can
            // see a statement before it is complete, which means changing the
            // parser to report "incomplete" as distinct from "invalid". That is
            // a real change and is not made here.
            let mut buf = String::new();
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_ok() {
                sql = buf;
            }
        }
        return if testsuite::run(&mut conn, &sql, &mut out) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

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
        "select", "insert", "update", "delete", "create", "drop", "alter", "begin", "commit",
        "rollback", "pragma", "explain", "with", "values", "replace",
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
