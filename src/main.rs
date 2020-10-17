#![forbid(unsafe_code)]
#![warn(future_incompatible, rust_2018_idioms, single_use_lifetimes, unreachable_pub)]
#![warn(clippy::all, clippy::default_trait_access)]
// mem::take and #[non_exhaustive] requires Rust 1.40, matches! requires Rust 1.42
#![allow(
    clippy::mem_replace_with_default,
    clippy::manual_non_exhaustive,
    clippy::match_like_matches_macro
)]

#[macro_use]
mod term;

mod cli;
mod manifest;
mod metadata;
mod package;
mod process;
mod remove_dev_deps;
mod restore;

use anyhow::{bail, Context, Error};
use std::{env, ffi::OsString, fs, path::Path};

use crate::{
    cli::{Args, Coloring},
    manifest::{find_root_manifest_for_wd, Manifest},
    metadata::Metadata,
    package::{Kind, Package, Progress},
    process::ProcessBuilder,
    restore::Restore,
};

type Result<T, E = Error> = std::result::Result<T, E>;

fn main() {
    let mut coloring = None;
    if let Err(e) = try_main(&mut coloring) {
        error!(coloring, "{:#}", e);
        std::process::exit(1)
    }
}

fn try_main(coloring: &mut Option<Coloring>) -> Result<()> {
    let args = cli::RawArgs::new();
    let args = cli::args(&args, coloring)?.unwrap_or_else(|| std::process::exit(0));
    let metadata = Metadata::new(&args)?;

    let current_manifest = match &args.manifest_path {
        Some(path) => Manifest::new(Path::new(path))?,
        None => Manifest::new(find_root_manifest_for_wd(&env::current_dir()?)?)?,
    };

    // TODO: Ideally, we should do this, but for now, we allow it as cargo-hack
    // may mistakenly interpret the specified valid feature flag as unknown.
    // if args.ignore_unknown_features && !args.workspace && !current_manifest.is_virtual() {
    //     bail!(
    //         "--ignore-unknown-features can only be used in the root of a virtual workspace or together with --workspace"
    //     )
    // }

    exec_on_workspace(&args, &current_manifest, &metadata)
}

fn exec_on_workspace(
    args: &Args<'_>,
    current_manifest: &Manifest,
    metadata: &Metadata,
) -> Result<()> {
    assert!(
        args.subcommand.is_some() || args.remove_dev_deps,
        "no subcommand or valid flag specified"
    );

    let restore = Restore::new(args);
    let mut line = ProcessBuilder::from_args(cargo_binary(), args);

    if let Some(color) = args.color {
        line.arg("--color");
        line.arg(color.as_str());
    }

    let mut progress = Progress::default();
    let packages = if args.workspace {
        args.exclude.iter().for_each(|spec| {
            if !metadata.packages.iter().any(|package| package.name == *spec) {
                warn!(
                    args.color,
                    "excluded package(s) {} not found in workspace `{}`",
                    spec,
                    metadata.workspace_root.display()
                );
            }
        });

        let packages =
            metadata.packages.iter().filter(|package| !args.exclude.contains(&&*package.name));
        Package::from_iter(args, packages, &mut progress)?
    } else if !args.package.is_empty() {
        if let Some(spec) = args
            .package
            .iter()
            .find(|spec| !metadata.packages.iter().any(|package| package.name == **spec))
        {
            bail!("package ID specification `{}` matched no packages", spec)
        }

        let packages =
            metadata.packages.iter().filter(|package| args.package.contains(&&*package.name));
        Package::from_iter(args, packages, &mut progress)?
    } else if current_manifest.is_virtual() {
        Package::from_iter(args, &metadata.packages, &mut progress)?
    } else {
        let current_package = current_manifest.package_name();
        let package = metadata.packages.iter().find(|package| package.name == *current_package);
        Package::from_iter(args, package, &mut progress)?
    };

    packages
        .iter()
        .try_for_each(|package| exec_on_package(args, package, &line, &restore, &mut progress))
}

fn exec_on_package(
    args: &Args<'_>,
    package: &Package<'_>,
    line: &ProcessBuilder<'_>,
    restore: &Restore,
    progress: &mut Progress,
) -> Result<()> {
    if let Kind::SkipAsPrivate = package.kind {
        info!(args.color, "skipped running on private crate {}", package.name_verbose(args));
        Ok(())
    } else {
        let mut line = line.clone();
        line.append_features_from_args(args, package);

        line.arg("--manifest-path");
        line.arg(&package.manifest_path);

        no_dev_deps(args, package, &mut line, restore, progress)
    }
}

fn no_dev_deps(
    args: &Args<'_>,
    package: &Package<'_>,
    line: &mut ProcessBuilder<'_>,
    restore: &Restore,
    progress: &mut Progress,
) -> Result<()> {
    if args.no_dev_deps || args.remove_dev_deps {
        let new = package.manifest.remove_dev_deps();
        let mut handle = restore.set_manifest(&package.manifest);

        fs::write(&package.manifest_path, new).with_context(|| {
            format!("failed to update manifest file: {}", package.manifest_path.display())
        })?;

        package::exec(args, package, line, progress)?;

        handle.done()
    } else {
        package::exec(args, package, line, progress)
    }
}

fn cargo_binary() -> OsString {
    env::var_os("CARGO_HACK_CARGO_SRC")
        .unwrap_or_else(|| env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}
