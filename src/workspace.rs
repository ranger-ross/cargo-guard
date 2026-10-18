use std::path::Path;

use cargo_metadata::{Metadata, Package};
use eyre::Context;

pub fn is_member(metadata: &Metadata, package: &Package) -> bool {
    metadata.workspace_members.contains(&package.id)
}

/// Check exact membership in the consuming workspace.
pub fn contains_manifest(metadata: &Metadata, manifest: &Path) -> eyre::Result<bool> {
    let manifest = manifest
        .canonicalize()
        .with_context(|| format!("could not resolve manifest {}", manifest.display()))?;
    for package in &metadata.packages {
        if is_member(metadata, package)
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
