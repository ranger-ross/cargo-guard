use std::ffi::OsString;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cargo-guard", version, about = "Cargo build script allow list")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactively review and approve build scripts
    Review {
        /// Package to review; omit to review all packages
        package: Option<String>,
        #[command(flatten)]
        options: ReviewOptions,
    },
    // Cargo's host runner invokes us with an executable path followed by
    // arbitrary arguments. Clap leaves those arguments uninterpreted.
    #[command(external_subcommand)]
    BuildScript(Vec<OsString>),
}

#[derive(Clone, Debug, Default, clap::Args)]
pub struct ReviewOptions {
    /// Filter metadata to this target triple (default: all platforms)
    #[arg(long)]
    pub target: Option<String>,
    /// Workspace features to enable (comma/space separated; may be repeated)
    #[arg(long, short = 'F', value_delimiter = ',')]
    pub features: Vec<String>,
    /// Enable all features
    #[arg(long)]
    pub all_features: bool,
    /// Disable default features
    #[arg(long)]
    pub no_default_features: bool,
}

impl Args {
    pub fn parse_args() -> Self {
        Self::try_parse_args_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
    }

    fn try_parse_args_from(
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, clap::Error> {
        let mut args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        // `cargo guard ...` invokes `cargo-guard guard ...`.
        if args.get(1).is_some_and(|arg| arg == "guard") {
            args.remove(1);
        }
        // Clap requires UTF-8 for the external subcommand name itself, even
        // with OsString arguments. A non-UTF-8 executable is always a runner.
        if args.get(1).is_some_and(|arg| arg.to_str().is_none()) {
            return Ok(Self {
                command: Command::BuildScript(args.into_iter().skip(1).collect()),
            });
        }
        Self::try_parse_from(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_and_cargo_review_commands() {
        for args in [
            vec!["cargo-guard", "review", "example"],
            vec!["cargo-guard", "guard", "review", "example"],
        ] {
            assert!(matches!(
                Args::try_parse_args_from(args).unwrap().command,
                Command::Review { package: Some(name), .. } if name == "example"
            ));
        }
        assert!(matches!(
            Args::try_parse_args_from(["cargo-guard", "guard", "review"])
                .unwrap()
                .command,
            Command::Review { package: None, .. }
        ));
    }

    #[test]
    fn parses_review_target_and_feature_selection() {
        let args = Args::try_parse_args_from([
            "cargo-guard",
            "guard",
            "review",
            "example",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--features",
            "one,two",
            "-F",
            "three four",
            "--all-features",
            "--no-default-features",
        ])
        .unwrap();
        let Command::Review { package, options } = args.command else {
            panic!("expected review")
        };
        assert_eq!(package.as_deref(), Some("example"));
        assert_eq!(options.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(options.features, ["one", "two", "three four"]);
        assert!(options.all_features && options.no_default_features);
    }

    #[test]
    fn preserves_runner_arguments_without_parsing_flags() {
        let forwarded = [
            "/path with spaces/build-script",
            "--help",
            "--version",
            "-x",
            "--",
            "guard",
            "review",
        ];
        let args =
            Args::try_parse_args_from(std::iter::once("cargo-guard").chain(forwarded)).unwrap();
        let Command::BuildScript(args) = args.command else {
            panic!("expected runner")
        };
        assert_eq!(args, forwarded.map(OsString::from));
    }

    #[test]
    fn reports_cli_usage_errors_and_help() {
        assert!(Args::try_parse_args_from(["cargo-guard"]).is_err());
        assert!(Args::try_parse_args_from(["cargo-guard", "review", "--unknown"]).is_err());
        assert!(Args::try_parse_args_from(["cargo-guard", "review", "one", "two"]).is_err());
        assert_eq!(
            Args::try_parse_args_from(["cargo-guard", "guard", "review", "--help"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::DisplayHelp
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_runner_paths_and_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"/tmp/build-\xff".to_vec());
        let arg = OsString::from_vec(b"arg-\xfe".to_vec());
        let args =
            Args::try_parse_args_from([OsString::from("cargo-guard"), path.clone(), arg.clone()])
                .unwrap();
        let Command::BuildScript(args) = args.command else {
            panic!("expected runner")
        };
        assert_eq!(args, [path, arg]);
    }
}
