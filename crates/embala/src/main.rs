mod config;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use config::Config;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build installer packages from prebuilt artifacts
    Build {
        /// Path to the embala config file
        #[arg(long, default_value = "embala.toml")]
        config: PathBuf,
        /// Formats to build, comma-separated (default: every format with a
        /// section in the config)
        #[arg(long, value_delimiter = ',')]
        formats: Vec<Format>,
        /// Directory the artifacts are written into
        #[arg(long, default_value = "dist")]
        out_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Msi,
    App,
    Pkg,
    Nupkg,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Msi => "msi",
            Format::App => "app",
            Format::Pkg => "pkg",
            Format::Nupkg => "nupkg",
        })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            config,
            formats,
            out_dir,
        } => build(&config, &formats, &out_dir),
    }
}

fn build(config_path: &Path, formats: &[Format], _out_dir: &Path) -> Result<()> {
    let config = Config::load(config_path)?;

    let formats: Vec<Format> = if formats.is_empty() {
        let configured = [
            (Format::Msi, config.msi.is_some()),
            (Format::App, config.app.is_some()),
            (Format::Pkg, config.pkg.is_some()),
            (Format::Nupkg, config.nupkg.is_some()),
        ];
        let present: Vec<Format> = configured
            .iter()
            .filter(|(_, some)| *some)
            .map(|(format, _)| *format)
            .collect();
        if present.is_empty() {
            bail!(
                "no format sections ([msi], [app], [pkg], [nupkg]) in {}",
                config_path.display()
            );
        }
        present
    } else {
        formats.to_vec()
    };

    for format in formats {
        let section_present = match format {
            Format::Msi => config.msi.is_some(),
            Format::App => config.app.is_some(),
            Format::Pkg => config.pkg.is_some(),
            Format::Nupkg => config.nupkg.is_some(),
        };
        if !section_present {
            bail!(
                "{format} requested but the config has no [{format}] section ({})",
                config_path.display()
            );
        }
        build_format(format, &config, _out_dir)?;
    }
    Ok(())
}

fn build_format(format: Format, _config: &Config, _out_dir: &Path) -> Result<()> {
    match format {
        Format::Msi => bail!("msi: not implemented yet"),
        Format::App => bail!("app: not implemented yet"),
        Format::Pkg => bail!("pkg: not implemented yet"),
        Format::Nupkg => bail!("nupkg: not implemented yet"),
    }
}
