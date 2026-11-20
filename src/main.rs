use std::env;
use std::path::PathBuf;
use std::process::{self, Command};
use std::time::Duration;

use vdash::approve;
use vdash::config::{self, BackendKind};
use vdash::error::{VdashError, Result};
use vdash::fingerprint;
use vdash::gate;
use vdash::lean;
use vdash::paths::{doctor, find_campaign, init_campaign, Campaign, Ctx};
use vdash::proc;
use clap::{Parser, Subcommand};

const CLI_BANNER: &str = "\
  ████╗                vdash@lean4
  ██╔═╝                ───────────
  ██║        ██╗   ██╗ App:      AlphaProof Code, rebuilt for Claude & Gemini
  █████████╗ ██║   ██║ Backends: claude · gemini (agy)
  ██╔══════╝ ╚██╗ ██╔╝ Verify:   Lean 4 kernel · leanchecker · held-out spec
  ██║         ╚████╔╝  Search:   Mathlib + Loogle (vdash search)
  ██║          ╚═══╝   Trust:    only the gatekeeper accepts a proof
  ╚═╝                  ███ ███ ███ ███ ███ ███ ███ ███";

#[derive(Parser, Debug)]
#[command(
    name = "vdash",
    version = "0.1.0",
    before_help = CLI_BANNER,
    about = "Vdash (⊢): a team of AI agents proving Lean theorems against a spec you approve"
)]
struct Cli {
    /// Campaign directory
    #[arg(short = 'C', global = true, value_name = "DIR")]
    campaign_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a campaign
    Init {
        dir: PathBuf,
        #[arg(long)]
        host: PathBuf,
        #[arg(long)]
        example: Option<String>,
    },
    /// Check external tools and logins
    Doctor,
    /// Build deps, elaborate file, print diagnostics
    Check {
        file: PathBuf,
    },
    /// Existence + kind + axioms of declarations
    Axioms {
        file: PathBuf,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Print the closure of every target
    Tcb,
    /// Baseline commit, held-out copy, anchor
    Approve,
    /// Reopen a finished or approved campaign in the spec phase to add goals
    Restart,
    /// Run the gatekeeper
    Gate {
        #[arg(long)]
        json: bool,
        /// Ignore the gate cache in state_dir and rebuild from scratch
        #[arg(long)]
        fresh: bool,
    },
    /// Run the engine
    Prove {
        #[arg(long)]
        rounds: Option<u32>,
        #[arg(long)]
        single: bool,
        #[arg(long)]
        detach: bool,
    },
    /// Plain-text summary
    Status,
    /// Pause a running engine
    Pause,
    /// Resume a paused engine
    Resume,
    /// Stop a running engine
    Stop,
    /// Kill the engine and every agent subprocess now, for good
    Kill,
    /// Spec phase commands
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },
    /// Amendment commands
    Amend {
        #[command(subcommand)]
        action: AmendAction,
    },
    /// List open escalations (questions for you), or show one in full
    Escalations {
        /// Also list resolved and closed ones
        #[arg(long)]
        all: bool,
        /// Show escalation N's file
        n: Option<String>,
    },
    /// Close escalations, e.g. `vdash resolve 23 "use them as cited hypotheses"` or
    /// `vdash resolve 1-15`. An answer is also queued as guidance for the orchestrator.
    Resolve {
        /// Numbers or ranges: 23, 16-22, 1-3,7
        which: String,
        /// Your answer (omit to dismiss)
        answer: Vec<String>,
    },
    /// Side question to the monitor agent
    Btw {
        question: String,
    },
    /// Sign in to your Claude or Gemini account
    Login {
        /// Backend to sign in to (`claude` or `gemini`); omit to check login status
        backend: Option<String>,
    },
    /// Show or change which backend and model serves each role
    Model {
        /// Optional `default` (restore the built-in Claude/Gemini split),
        /// `refresh [--quiet]` (reread `agy models`, recheck rejected models) or
        /// `<role|all|claude|gemini> <backend[:model]|model>`
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Search Mathlib, campaign Lean files, and online Loogle documentation
    Search {
        /// Skip online Loogle query and search only local Lean/Mathlib files
        #[arg(long)]
        offline: bool,
        #[arg(required = true)]
        query: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SpecAction {
    /// Draft the initial specification, then revise until the reviewers say ok
    Draft {
        /// Run one step only (no revision loop)
        #[arg(long)]
        once: bool,
    },
    /// Revise the specification with feedback, then loop until the reviewers say ok
    Revise {
        text: String,
        /// Run one step only (no revision loop)
        #[arg(long)]
        once: bool,
    },
    /// Rerun the specification reviewers, then revise until they say ok
    Review {
        /// Run the reviewers only (no revision loop)
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AmendAction {
    /// List amendments
    List,
    /// Approve an amendment
    Approve {
        n: u32,
        /// Required to apply an escalated proposal: accept that it weakens the guarantee
        #[arg(long)]
        accept_weakening: bool,
    },
    /// Reject an amendment
    Reject {
        n: u32,
    },
    /// Submit an amendment proposal (JSON file) to the critic
    Propose {
        file: PathBuf,
    },
}

/// `VDASH_CAMPAIGN`, or the pre-rename `APC_CAMPAIGN`.
fn campaign_env() -> Option<String> {
    env::var("VDASH_CAMPAIGN").ok().filter(|v| !v.is_empty()).or_else(|| env::var("APC_CAMPAIGN").ok())
}

fn locate_campaign(explicit: Option<&std::path::Path>) -> Result<Campaign> {
    let env_val = campaign_env();
    let cwd = env::current_dir().map_err(|e| VdashError::new(2, format!("cannot read cwd: {e}")))?;
    find_campaign(explicit, env_val.as_deref(), &cwd)
}

/// `$XDG_CACHE_HOME/vdash/models.json`, else `~/.cache/vdash/models.json`.
fn models_cache_path() -> PathBuf {
    let base = match env::var("XDG_CACHE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())).join(".cache"),
    };
    base.join("vdash").join("models.json")
}

fn build_ctx(explicit: Option<&std::path::Path>) -> Result<(Ctx, PathBuf)> {
    let cwd = env::current_dir().map_err(|e| VdashError::new(2, format!("cannot read cwd: {e}")))?;
    let env_val = campaign_env();
    let campaign = find_campaign(explicit, env_val.as_deref(), &cwd)?;
    let cfg = config::load(&campaign.root)?;
    let state_base = if let Ok(xdg) = env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            PathBuf::from(xdg)
        } else {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
                .join(".local/state")
        }
    } else {
        PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())).join(".local/state")
    };
    // Campaigns approved before the rename keep their trust anchor under `apc/`.
    let old = state_base.join("apc").join(&cfg.id);
    let state_dir = if old.is_dir() && !state_base.join("vdash").join(&cfg.id).exists() {
        old
    } else {
        state_base.join("vdash").join(&cfg.id)
    };
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("vdash"));
    Ok((
        Ctx {
            campaign,
            cfg,
            state_dir,
            exe,
            models_cache: models_cache_path(),
        },
        cwd,
    ))
}

fn detect_available_backends() -> Vec<BackendKind> {
    let mut found = Vec::new();
    for (kind, bin) in [
        (BackendKind::Claude, "claude"),
        (BackendKind::Gemini, "agy"),
    ] {
        let mut cmd = Command::new(bin);
        cmd.arg("--version");
        if let Ok(out) = proc::run(&mut cmd, None, Duration::from_secs(5)) {
            if !out.timed_out && out.status == Some(0) {
                found.push(kind);
            }
        }
    }
    found
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Commands::Init { dir, host, example }) => {
            let backends = detect_available_backends();
            if let Some(notice) = init_campaign(&dir, &host, example.as_deref(), &backends)? {
                println!("vdash: {notice}");
            }
            Ok(())
        }
        Some(Commands::Doctor) => {
            let campaign = locate_campaign(cli.campaign_dir.as_deref())?;
            let cfg = config::load(&campaign.root)?;
            let (report, ok) = doctor(&cfg);
            print!("{report}");
            if ok {
                Ok(())
            } else {
                Err(VdashError::new(1, "doctor checks failed"))
            }
        }
        Some(Commands::Check { file }) => {
            let (ctx, cwd) = build_ctx(cli.campaign_dir.as_deref())?;
            let (report, ok) = lean::build::check_file(&ctx, &file, &cwd)?;
            print!("{report}");
            if ok {
                Ok(())
            } else {
                process::exit(1);
            }
        }
        Some(Commands::Axioms { file, names }) => {
            let (ctx, cwd) = build_ctx(cli.campaign_dir.as_deref())?;
            let (report, ok) = fingerprint::axioms_cmd(&ctx, &file, &names, &cwd)?;
            print!("{report}");
            if ok {
                Ok(())
            } else {
                process::exit(1);
            }
        }
        Some(Commands::Tcb) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let report = fingerprint::tcb_cmd(&ctx)?;
            print!("{report}");
            Ok(())
        }
        Some(Commands::Approve) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let out = approve::approve(&ctx)?;
            print!("{out}");
            Ok(())
        }
        Some(Commands::Restart) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let out = approve::restart(&ctx)?;
            print!("{out}");
            Ok(())
        }
        Some(Commands::Gate { json, fresh }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let (out, code) = gate::gate_cmd_opts(&ctx, json, fresh)?;
            print!("{out}");
            if code == 0 {
                Ok(())
            } else {
                process::exit(code as i32);
            }
        }
        Some(Commands::Prove { rounds, single, detach }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            if detach {
                return detach_engine(&ctx, rounds, single);
            }
            if ctx.cfg.backends.contains_key(&config::BackendKind::Gemini)
                && vdash::models::gemini_stale(&vdash::models::load(&ctx.models_cache), vdash::models::now_unix())
            {
                // Best effort: the engine warns about configured models from this list.
                let _ = vdash::models::refresh(&ctx, false);
            }
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            vdash::engine::install_signal_handlers(stop.clone());
            let pool = vdash::backend::pool::Pool::from_config(&ctx);
            let code = vdash::engine::run(ctx, &pool, vdash::engine::Opts { rounds, single }, stop)?;
            if code == 0 {
                println!("vdash: every target is accepted");
                Ok(())
            } else {
                process::exit(code as i32);
            }
        }
        Some(Commands::Btw { question }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let pool = vdash::backend::pool::Pool::from_config(&ctx);
            print!("{}", vdash::monitor::ask(&ctx, &pool, &question)?);
            Ok(())
        }
        Some(Commands::Login { backend }) => {
            let cfg = locate_campaign(cli.campaign_dir.as_deref())
                .ok()
                .and_then(|c| config::load(&c.root).ok());
            match backend.as_deref() {
                None => {
                    println!("{}", vdash::tui::login_status(cfg.as_ref()));
                    Ok(())
                }
                Some(b) => {
                    let msg = vdash::tui::login_interactive(cfg.as_ref(), b)?;
                    println!("vdash: {msg}");
                    Ok(())
                }
            }
        }
        Some(Commands::Model { args }) if args.first().map(String::as_str) == Some("refresh") => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            // `--quiet` is the app's startup refresh: the Gemini list only, no output.
            let quiet = match &args[1..] {
                [] => false,
                [q] if q == "--quiet" => true,
                _ => return Err(VdashError::new(2, "usage: vdash model refresh [--quiet]")),
            };
            let lines = vdash::models::refresh(&ctx, !quiet)?;
            if !quiet {
                for l in lines {
                    println!("vdash: {l}");
                }
                let cache = vdash::models::load(&ctx.models_cache);
                print!("{}", vdash::models::catalog_text(&vdash::models::catalog(&cache, vdash::models::now_unix())));
                for w in vdash::models::role_warnings(&ctx.cfg, &cache, vdash::models::now_unix()) {
                    println!("warning: {w}");
                }
            }
            Ok(())
        }
        Some(Commands::Model { args }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let cmd_line = if args.is_empty() {
                "/model".to_string()
            } else {
                format!("/model {}", args.join(" "))
            };
            match vdash::tui::commands::parse(&cmd_line) {
                Ok(vdash::tui::commands::Input::Cmd(vdash::tui::commands::Cmd::Model(None))) => {
                    for l in vdash::tui::role_lines(&ctx) {
                        println!("{l}");
                    }
                    print!("\n{}", vdash::models::catalog_text(&vdash::models::current(&ctx.models_cache)));
                    Ok(())
                }
                Ok(vdash::tui::commands::Input::Cmd(vdash::tui::commands::Cmd::Model(Some((role, entry))))) => {
                    let changes = config::model_change(&ctx.cfg, &role, &entry)?;
                    let running = vdash::events::engine_running(&ctx).is_some();
                    for (r, es) in &changes {
                        if running {
                            let entries = es.iter().map(|e| e.to_string()).collect();
                            vdash::events::send(&ctx, &vdash::events::Control::SetRole { role: r.to_string(), entries })?;
                        } else {
                            config::set_role(&ctx.campaign.root, *r, es)?;
                        }
                    }
                    if !running {
                        vdash::engine::record_engine_file(&ctx, "campaign.toml");
                    }
                    for (r, es) in &changes {
                        println!("vdash: {r} → {}", es.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(", "));
                    }
                    Ok(())
                }
                Ok(vdash::tui::commands::Input::Cmd(vdash::tui::commands::Cmd::ModelDefault)) => {
                    if vdash::events::engine_running(&ctx).is_some() {
                        for r in config::Role::ALL {
                            let entries = config::default_role_entries(r).iter().map(|e| e.to_string()).collect();
                            vdash::events::send(&ctx, &vdash::events::Control::SetRole { role: r.to_string(), entries })?;
                        }
                        println!("vdash: default split sent to the running engine");
                    } else {
                        config::reset_roles(&ctx.campaign.root)?;
                        vdash::engine::record_engine_file(&ctx, "campaign.toml");
                        println!("vdash: default split restored in campaign.toml");
                    }
                    println!("{}", config::default_split_summary());
                    Ok(())
                }
                Err(e) => Err(VdashError::new(2, e)),
                _ => unreachable!(),
            }
        }
        Some(Commands::Search { offline, query }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let q = query.join(" ");
            let out = vdash::search::search_cmd(&ctx, &q, offline)?;
            print!("{out}");
            Ok(())
        }
        Some(Commands::Status) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            print!("{}", vdash::status::render(&ctx)?);
            Ok(())
        }
        Some(Commands::Kill) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            println!("{}", vdash::kill::kill(&ctx)?);
            Ok(())
        }
        Some(Commands::Pause) | Some(Commands::Resume) | Some(Commands::Stop) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            if vdash::events::engine_running(&ctx).is_none() {
                return Err(VdashError::new(2, "no engine is running for this campaign"));
            }
            let (c, word) = match cli.command {
                Some(Commands::Pause) => (vdash::events::Control::Pause, "pause"),
                Some(Commands::Resume) => (vdash::events::Control::Resume, "resume"),
                _ => (vdash::events::Control::Stop, "stop"),
            };
            vdash::events::send(&ctx, &c)?;
            println!("vdash: {word} sent; the engine acts on it before its next agent or stage");
            Ok(())
        }
        Some(Commands::Spec { action }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let pool = vdash::backend::pool::Pool::from_config(&ctx);
            use vdash::spec::FirstStep;
            let report = match action {
                SpecAction::Draft { once: true } => vdash::spec::draft(&ctx, &pool)?,
                SpecAction::Revise { text, once: true } => vdash::spec::revise(&ctx, &pool, &text)?,
                SpecAction::Review { once: true } => vdash::spec::review_only(&ctx, &pool)?,
                SpecAction::Draft { once: false } => vdash::spec::iterate(&ctx, &pool, FirstStep::Draft)?.0,
                SpecAction::Revise { text, once: false } => vdash::spec::iterate(&ctx, &pool, FirstStep::Revise(text))?.0,
                SpecAction::Review { once: false } => vdash::spec::iterate(&ctx, &pool, FirstStep::Review)?.0,
            };
            print!("{}", report.summary);
            Ok(())
        }
        Some(Commands::Amend { action }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let out = match action {
                AmendAction::List => vdash::amend::list_cmd(&ctx)?,
                AmendAction::Approve { n, accept_weakening } => vdash::amend::approve_cmd(&ctx, n, accept_weakening)?,
                AmendAction::Reject { n } => vdash::amend::reject_cmd(&ctx, n)?,
                AmendAction::Propose { file } => {
                    let pool = vdash::backend::pool::Pool::from_config(&ctx);
                    vdash::amend::propose_cmd(&ctx, &pool, &file)?
                }
            };
            print!("{out}");
            Ok(())
        }
        Some(Commands::Escalations { all, n }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            let out = match n {
                Some(n) => {
                    let n: u32 = n.trim_start_matches('#').parse().map_err(|_| vdash::error::VdashError::new(2, format!("'{n}' is not an escalation number")))?;
                    vdash::escalation::show(&ctx, n)?
                }
                None => vdash::escalation::list_text(&ctx, all),
            };
            print!("{out}");
            Ok(())
        }
        Some(Commands::Resolve { which, answer }) => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            print!("{}", vdash::escalation::resolve_cmd(&ctx, &which, &answer.join(" "))?);
            Ok(())
        }
        None => {
            let (ctx, _) = build_ctx(cli.campaign_dir.as_deref())?;
            use std::io::IsTerminal;
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                vdash::tui::run(ctx)
            } else {
                print!("{}", vdash::status::render(&ctx)?);
                Ok(())
            }
        }
    }
}

/// Start `vdash prove` as a detached background process with output to
/// `logs/engine.out`, and return at once.
fn detach_engine(ctx: &Ctx, rounds: Option<u32>, single: bool) -> Result<()> {
    if let Some(pid) = vdash::events::engine_running(ctx) {
        return Err(VdashError::new(2, format!("an engine is already running (pid {pid})")));
    }
    let log = ctx.campaign.engine_out_file();
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d).map_err(|e| VdashError::io_path(d, e))?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| VdashError::io_path(&log, e))?;
    let err = out.try_clone().map_err(|e| VdashError::io_path(&log, e))?;
    let mut cmd = Command::new(&ctx.exe);
    cmd.arg("-C").arg(&ctx.campaign.root).arg("prove");
    if let Some(r) = rounds {
        cmd.arg("--rounds").arg(r.to_string());
    }
    if single {
        cmd.arg("--single");
    }
    use std::os::unix::process::CommandExt;
    cmd.stdin(std::process::Stdio::null()).stdout(out).stderr(err).process_group(0);
    let child = cmd.spawn().map_err(|e| VdashError::new(5, format!("could not start the engine: {e}")))?;
    println!("vdash: engine started in the background (pid {}); follow it with `vdash` or `vdash status`", child.id());
    Ok(())
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            info.to_string()
        };
        eprintln!("vdash: internal error: {msg}");
        process::exit(5);
    }));
}

fn main() {
    install_panic_hook();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    print!("{err}");
                    process::exit(0);
                }
                _ => {
                    eprintln!("vdash: error: {err}");
                    process::exit(2);
                }
            }
        }
    };

    if let Err(err) = run(cli) {
        eprintln!("vdash: error: {}", err.message);
        process::exit(err.code as i32);
    }
}
