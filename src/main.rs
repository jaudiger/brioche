use std::{path::PathBuf, process::ExitCode};

use anyhow::Context as _;
use brioche::{fs_utils, sandbox::SandboxExecutionConfig};
use clap::Parser;
use tracing::Instrument;

#[derive(Debug, Parser)]
enum Args {
    Build(BuildArgs),
    Check(CheckArgs),
    Lsp(LspArgs),
    RunSandbox(RunSandboxArgs),
}

const BRIOCHE_SANDBOX_ERROR_CODE: u8 = 122;

fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();

    match args {
        Args::Build(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            let exit_code = rt.block_on(build(args))?;

            Ok(exit_code)
        }
        Args::Check(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            let exit_code = rt.block_on(check(args))?;

            Ok(exit_code)
        }
        Args::Lsp(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            rt.block_on(lsp(args))?;

            Ok(ExitCode::SUCCESS)
        }
        Args::RunSandbox(args) => {
            let exit_code = run_sandbox(args);

            Ok(exit_code)
        }
    }
}

#[derive(Debug, Parser)]
struct BuildArgs {
    #[clap(short, long)]
    project: PathBuf,
    #[clap(short, long, default_value = "default")]
    export: String,
    #[clap(short, long)]
    output: Option<PathBuf>,
    #[clap(long)]
    check: bool,
    #[clap(long)]
    replace: bool,
    #[clap(long)]
    keep: bool,
}

async fn build(args: BuildArgs) -> anyhow::Result<ExitCode> {
    let (reporter, mut guard) = brioche::reporter::start_console_reporter()?;

    let brioche = brioche::brioche::BriocheBuilder::new(reporter.clone())
        .keep_temps(args.keep)
        .build()
        .await?;

    let build_future = async {
        let project = brioche::brioche::project::resolve_project(&brioche, &args.project).await?;

        if args.check {
            let checked = brioche::brioche::script::check::check(&brioche, &project).await?;

            let result =
                checked.ensure_ok(brioche::brioche::script::check::DiagnosticLevel::Warning);

            match result {
                Ok(()) => reporter.emit(superconsole::Lines::from_multiline_string(
                    "No errors found",
                    superconsole::style::ContentStyle {
                        foreground_color: Some(superconsole::style::Color::Green),
                        ..superconsole::style::ContentStyle::default()
                    },
                )),
                Err(diagnostics) => {
                    guard.shutdown_console().await;

                    diagnostics.write(&mut std::io::stdout())?;
                    return anyhow::Ok(ExitCode::FAILURE);
                }
            }
        }

        let value =
            brioche::brioche::script::evaluate::evaluate(&brioche, &project, &args.export).await?;
        let result = brioche::brioche::resolve::resolve(&brioche, value).await?;

        guard.shutdown_console().await;

        let result_hash = result.value.hash();
        println!("Result: {result_hash}");

        if let Some(output) = &args.output {
            if args.replace {
                fs_utils::try_remove(output)
                    .await
                    .with_context(|| format!("Failed to remove path {}", output.display()))?;
            }

            println!("Writing output");
            brioche::brioche::output::create_output(
                &brioche,
                &result.value,
                brioche::brioche::output::OutputOptions {
                    output_path: output,
                    merge: false,
                    resources_dir: None,
                },
            )
            .await?;
            println!("Wrote output to {}", output.display());
        }

        anyhow::Ok(ExitCode::SUCCESS)
    };

    let exit_code = build_future
        .instrument(tracing::info_span!("build", args = ?args))
        .await?;

    Ok(exit_code)
}

#[derive(Debug, Parser)]
struct CheckArgs {
    #[clap(short, long)]
    project: PathBuf,
}

async fn check(args: CheckArgs) -> anyhow::Result<ExitCode> {
    let (reporter, mut guard) = brioche::reporter::start_console_reporter()?;

    let brioche = brioche::brioche::BriocheBuilder::new(reporter)
        .build()
        .await?;

    let check_future = async {
        let project = brioche::brioche::project::resolve_project(&brioche, &args.project).await?;
        let checked = brioche::brioche::script::check::check(&brioche, &project).await?;

        guard.shutdown_console().await;

        let result = checked.ensure_ok(brioche::brioche::script::check::DiagnosticLevel::Message);

        match result {
            Ok(()) => {
                println!("No errors found 🎉");
                anyhow::Ok(ExitCode::SUCCESS)
            }
            Err(diagnostics) => {
                diagnostics.write(&mut std::io::stdout())?;
                anyhow::Ok(ExitCode::FAILURE)
            }
        }
    };

    let exit_code = check_future
        .instrument(tracing::info_span!("check", args = ?args))
        .await?;

    Ok(exit_code)
}

#[derive(Debug, Parser)]
struct LspArgs {
    /// Use stdio for LSP transport
    #[clap(long)]
    stdio: bool,
}

async fn lsp(_args: LspArgs) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let local_pool = tokio_util::task::LocalPoolHandle::new(5);

    let (service, socket) = tower_lsp::LspService::new(move |client| {
        let local_pool = &local_pool;
        futures::executor::block_on(async move {
            let (reporter, _guard) = brioche::reporter::start_lsp_reporter(client.clone());
            let brioche = brioche::brioche::BriocheBuilder::new(reporter)
                .build()
                .await?;
            let lsp_server =
                brioche::brioche::script::lsp::BriocheLspServer::new(local_pool, brioche, client)
                    .await?;
            anyhow::Ok(lsp_server)
        })
        .expect("failed to build LSP")
    });

    // Note: For now, we always use stdio for the LSP
    tower_lsp::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;

    Ok(())
}

#[derive(Debug, Parser)]
struct RunSandboxArgs {
    #[clap(long)]
    config: String,
}

fn run_sandbox(args: RunSandboxArgs) -> ExitCode {
    let config = match serde_json::from_str::<SandboxExecutionConfig>(&args.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("brioche: failed to parse sandbox config: {error:#}");
            return ExitCode::from(BRIOCHE_SANDBOX_ERROR_CODE);
        }
    };

    let status = match brioche::sandbox::run_sandbox(config) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("brioche: failed to run sandbox: {error:#}");
            return ExitCode::from(BRIOCHE_SANDBOX_ERROR_CODE);
        }
    };

    status
        .code()
        .and_then(|code| {
            let code: u8 = code.try_into().ok()?;
            Some(ExitCode::from(code))
        })
        .unwrap_or_else(|| {
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        })
}