use std::path::Path;

use cargo_metadata::{Metadata, MetadataCommand, Package};
use eyre::Context;

pub fn is_member(metadata: &Metadata, package: &Package) -> bool {
    metadata.workspace_members.contains(&package.id)
}

/// Ask Cargo about the consuming workspace, never the dependency's workspace.
pub fn contains_manifest(root: &Path, manifest: &Path) -> eyre::Result<bool> {
    let metadata = MetadataCommand::new()
        .manifest_path(root.join("Cargo.toml"))
        .no_deps()
        .other_options(vec!["--offline".into()])
        .exec()
        .context("could not determine consuming workspace members")?;
    let manifest = manifest
        .canonicalize()
        .with_context(|| format!("could not resolve manifest {}", manifest.display()))?;
    for package in &metadata.packages {
        if is_member(&metadata, package)
            && package
                .manifest_path
                .as_std_path()
                .canonicalize()
                .with_context(|| {
                    format!(
                        "could not resolve member manifest {}",
                        package.manifest_path
                    )
                })?
                == manifest
        {
            return Ok(true);
        }
    }
    Ok(false)
}
