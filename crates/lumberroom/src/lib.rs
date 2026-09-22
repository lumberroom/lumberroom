//! The lumberroom client: the half that runs on the machine holding the transcripts.
//!
//! It runs on the machine that holds the transcripts and talks to a lumberroom server that may be
//! somewhere else. It is a separate crate from the server for one reason: cargo features are
//! crate-wide, `fastembed` is a hard dependency of the server, and a client sharing that crate
//! would carry an ONNX runtime and 209MB of model weights to do work that needs neither.
//!
//! It shares `~/.config/lumberroom/config.json` with `bin/lumberroom.mjs` and matches that client's output and
//! exit codes, so the two can be swapped under the acceptance scripts.

pub mod args;
pub mod cleanup;
pub mod client;
pub mod commands;
pub mod config;
pub mod eval;
pub mod format;
pub mod import;
pub mod ingest;
pub mod oauth;
pub mod review;
pub mod sealed;
pub mod wire;

use std::io::Write;

use crate::args::Args;
use crate::client::{err, Client, Result};
use crate::config::{FileConfig, ProcessEnv};

pub fn out(line: &str) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
}

pub fn out_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => out(&s),
        Err(_) => out("null"),
    }
}

/// A question on the same line as the answer, so the terminal reads like node's readline prompt.
pub fn prompt(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{text}");
    let _ = stdout.flush();
}

const COMMANDS: &str =
    "doctor, whoami, login, clients, bootstrap, search, write, forget, review, supersede, fill-date, \
registry, stats, export, archive, ingest, import, cleanup, history, alias, seal, unseal, recall, tools, \
currency, arity, graph, \
hash-password, eval-longmemeval, version, help";

/// Parse, dispatch, and turn a failure into the exit code the scripts read.
pub async fn run(argv: Vec<String>) -> i32 {
    let args = Args::parse(argv);
    let env = ProcessEnv;
    let path = config::config_path(&env);
    let file = FileConfig::load(path);
    let resolved = config::resolve(
        &env,
        &file,
        args.value("url"),
        args.value("token"),
        args.value("invocation"),
        args.present("hook"),
        args.value("timeout"),
    );

    let client = match Client::new(resolved, file) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };

    let command = if args.present("version") {
        "version".to_string()
    } else if args.present("help") {
        "help".to_string()
    } else {
        args.positional_at(0).unwrap_or("doctor").to_string()
    };
    let result = dispatch(&client, &args, &command).await;
    match result {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

async fn dispatch(client: &Client, args: &Args, command: &str) -> Result<()> {
    match command {
        "doctor" => commands::doctor(client).await,
        "whoami" => commands::whoami(client, args).await,
        "login" => oauth::login(client, args).await,
        "clients" => commands::clients(client, args).await,
        "bootstrap" => {
            let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
            let project_env = std::env::var("CLAUDE_PROJECT_DIR").ok().filter(|s| !s.is_empty());
            commands::bootstrap(client, args, &cwd, project_env).await
        }
        "search" => commands::search(client, args).await,
        "write" => commands::write(client, args).await,
        "forget" => commands::forget(client, args, read_line).await,
        // `review::run` wants `&mut impl FnMut`; `read_line` is a bare fn item, so it is bound to
        // a local before the borrow rather than passed directly.
        "review" => {
            let mut read = read_line;
            review::run(client, args, &mut read).await
        }
        "supersede" => commands::supersede(client, args).await,
        "fill-date" => commands::fill_date(client, args).await,
        "currency" => commands::currency(client, args).await,
        "arity" => commands::arity(client, args).await,
        "graph" => commands::graph(client, args).await,
        "registry" => commands::registry(client, args).await,
        "stats" => commands::stats(client, args).await,
        "export" => commands::export(client, args).await,
        "archive" => commands::archive(client, args).await,
        "history" => commands::history(client, args).await,
        "alias" => commands::alias(client, args).await,
        "eval" => commands::eval(client, args).await,
        "eval-longmemeval" => eval::dispatch(client, args).await,
        "cleanup" => {
            let sub = args.positional_at(1).unwrap_or("run").to_string();
            cleanup::dispatch(client, args, &sub).await
        }
        "ingest" => {
            let sub = args.positional_at(1).unwrap_or("plan").to_string();
            ingest::dispatch(client, args, &sub).await
        }
        // No default subcommand. Import acts on a path the owner names, and guessing which of
        // several archives in a downloads directory was meant is a wrong import to undo by hand.
        "import" => {
            let sub = args
                .positional_at(1)
                .ok_or_else(|| {
                    err(format!("import needs a subcommand. Available: {}", import::SUBCOMMANDS))
                })?
                .to_string();
            import::dispatch(client, args, &sub).await
        }
        // Named rather than silently unsupported: the node CLI still has them and this says so.
        "seal" => sealed::seal(client, args, &ProcessEnv).await,
        "unseal" => sealed::unseal(client, args, &ProcessEnv).await,
        "version" => {
            out(&format!("lumberroom {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        "help" => {
            out(&format!("usage: lumberroom <command> [options]\n\ncommands: {COMMANDS}"));
            Ok(())
        }
        "recall" => commands::recall(client, args).await,
        "tools" => commands::tools(client).await,
        "hash-password" => commands::hash_password(),
        other => Err(err(format!("unknown command {other}. Try: {COMMANDS}"))),
    }
}

pub fn read_line() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf)
}

fn fail(e: client::CliError) -> i32 {
    let mut stderr = std::io::stderr();
    // The `lumberroom: ` prefix is node's, and it is kept: the acceptance scripts and the owner read
    // these lines, and matching them outranks naming the binary accurately in its own errors.
    let _ = writeln!(stderr, "lumberroom: {}", e.message);
    e.code
}
