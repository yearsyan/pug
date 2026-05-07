mod api;
mod config;
mod create_project;
mod engine;
mod extension;
mod platform;
mod project;
mod util;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "pug",
    version,
    about = "Godot custom engine and GDExtension manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Engine {
        #[command(subcommand)]
        command: EngineCommands,
    },
    Extension {
        #[command(subcommand)]
        command: ExtensionCommands,
    },
    Init(InitArgs),
    #[command(about = "Create a new Godot custom overlay project")]
    CreateProject(CreateProjectArgs),
    Login,
    SetupToken(SetupTokenArgs),
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
}

#[derive(Subcommand, Debug)]
enum EngineCommands {
    Build(EngineBuildArgs),
    List(EngineListArgs),
    Install(EngineInstallArgs),
    Use(EngineUseArgs),
    Current,
    Which(EngineWhichArgs),
    Start(EngineStartArgs),
    Uninstall(EngineTagArgs),
}

#[derive(Args, Debug)]
struct EngineBuildArgs {
    #[arg(long)]
    upload: bool,
    #[arg(long = "template-platform", visible_alias = "template-platforms")]
    template_platforms: Option<String>,
    #[arg(long)]
    godot_source: Option<PathBuf>,
    #[arg(long)]
    skip_patches: bool,
    #[arg(long)]
    no_restore: bool,
    #[arg(long)]
    no_log: bool,
    #[arg(long)]
    force: bool,
    #[arg(last = true)]
    scons_args: Vec<String>,
}

#[derive(Args, Debug)]
struct EngineListArgs {
    #[arg(long)]
    remote: bool,
}

#[derive(Args, Debug)]
struct EngineInstallArgs {
    tag: Option<String>,
    #[arg(long)]
    download_only: bool,
}

#[derive(Args, Debug)]
struct EngineUseArgs {
    tag: Option<String>,
}

#[derive(Args, Debug)]
struct EngineTagArgs {
    tag: String,
}

#[derive(Args, Debug)]
struct EngineWhichArgs {
    #[arg(long)]
    with_engine: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct EngineStartArgs {
    #[arg(long)]
    project: Option<PathBuf>,
    #[arg(long)]
    with_engine: Option<PathBuf>,
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum ExtensionCommands {
    Build(ExtensionBuildArgs),
    List(ExtensionListArgs),
}

#[derive(Args, Debug)]
struct ExtensionBuildArgs {
    #[arg(long)]
    upload: bool,
    #[arg(long)]
    platform: Option<String>,
    #[arg(long)]
    with_engine: Option<PathBuf>,
    #[arg(long)]
    debug: bool,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct ExtensionListArgs {
    #[arg(long)]
    remote: bool,
}

#[derive(Args, Debug)]
struct InitArgs {
    #[arg(long)]
    engine_tag: Option<String>,
    #[arg(long)]
    platforms: Option<String>,
}

#[derive(Args, Debug)]
struct CreateProjectArgs {
    #[arg(help = "Project name; prompts when omitted")]
    name: Option<String>,
    #[arg(
        long,
        help = "Git/HTTPS template repository to copy modules/ and patches/ from"
    )]
    template: Option<String>,
    #[arg(long, requires = "template", conflicts_with_all = ["tag", "commit"], help = "Template branch to clone")]
    branch: Option<String>,
    #[arg(long, requires = "template", conflicts_with_all = ["branch", "commit"], help = "Template tag to clone")]
    tag: Option<String>,
    #[arg(long, requires = "template", conflicts_with_all = ["branch", "tag"], help = "Template commit to checkout")]
    commit: Option<String>,
}

#[derive(Args, Debug)]
struct SetupTokenArgs {
    token: Option<String>,
}

#[derive(Subcommand, Debug)]
enum ProjectCommands {
    Install(ProjectInstallArgs),
    Export(ProjectExportArgs),
}

#[derive(Args, Debug)]
struct ProjectInstallArgs {
    package: Option<String>,
}

#[derive(Args, Debug)]
struct ProjectExportArgs {
    #[arg(long)]
    platform: Option<String>,
    #[arg(long)]
    android: bool,
    #[arg(long)]
    ios: bool,
    #[arg(long)]
    debug: bool,
    #[arg(long)]
    release: bool,
    #[arg(long)]
    with_engine: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Engine { command } => match command {
            EngineCommands::Build(args) => engine::build(engine::EngineBuildOptions {
                upload: args.upload,
                template_platforms: args.template_platforms,
                godot_source: args.godot_source,
                skip_patches: args.skip_patches,
                no_restore: args.no_restore,
                no_log: args.no_log,
                force: args.force,
                scons_args: args.scons_args,
            }),
            EngineCommands::List(args) => engine::list(args.remote),
            EngineCommands::Install(args) => engine::install(args.tag, args.download_only),
            EngineCommands::Use(args) => engine::use_tag(args.tag.as_deref()),
            EngineCommands::Current => engine::current(),
            EngineCommands::Which(args) => {
                let path = engine::resolve_editor(args.with_engine.as_deref())?;
                println!("{}", path.display());
                Ok(())
            }
            EngineCommands::Start(args) => engine::start(
                args.with_engine.as_deref(),
                args.project.as_deref(),
                &args.args,
            ),
            EngineCommands::Uninstall(args) => engine::uninstall(&args.tag),
        },
        Commands::Extension { command } => match command {
            ExtensionCommands::Build(args) => extension::build(extension::ExtensionBuildOptions {
                upload: args.upload,
                platform: args.platform,
                with_engine: args.with_engine,
                debug: args.debug,
                force: args.force,
            }),
            ExtensionCommands::List(args) => extension::list(args.remote),
        },
        Commands::Init(args) => project::init(args.engine_tag, args.platforms),
        Commands::CreateProject(args) => {
            create_project::create(args.name, args.template, args.branch, args.tag, args.commit)
        }
        Commands::Login => config::login(),
        Commands::SetupToken(args) => config::setup_access_token(args.token),
        Commands::Project { command } => match command {
            ProjectCommands::Install(args) => project::install(args.package.as_deref()),
            ProjectCommands::Export(args) => {
                project::export_project(project::ProjectExportOptions {
                    platform: args.platform,
                    android: args.android,
                    ios: args.ios,
                    debug: args.debug,
                    release: args.release,
                    with_engine: args.with_engine,
                })
            }
        },
    }
}
