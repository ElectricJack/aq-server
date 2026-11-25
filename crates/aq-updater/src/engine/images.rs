//! Select and prune obsolete release images.

use super::{Check, STATE_FORMAT_LABEL, rt};
use crate::runtime::Runtime;
use aq_release::ReleaseManifest;

pub(super) async fn pull_release<R: Runtime>(runtime: &R, target: &ReleaseManifest) -> Check {
    let image = &target.images.server;
    if let Err(e) = rt(runtime.pull(image).await)? {
        return Ok(Err(format!("pull of {image} failed: {e}")));
    }
    let want = target.state_format.to_string();
    Ok(match rt(runtime.image_labels(image).await)? {
        Ok(labels) if labels.get(STATE_FORMAT_LABEL) == Some(&want) => Ok(()),
        Ok(labels) => Err(format!(
            "image {image} has label {STATE_FORMAT_LABEL}={:?} but the manifest says {want}",
            labels.get(STATE_FORMAT_LABEL)
        )),
        Err(e) => Err(format!("cannot read labels of {image}: {e}")),
    })
}

/// Repository part of a digest reference (`host/path@sha256:…` → `host/path`).
pub fn image_repository(image: &str) -> Option<&str> {
    image.split_once('@').map(|(repo, _)| repo)
}

/// Images to remove (DEP7c): every image in `present` that is not in `keep`
/// (the current and previous release's images). `present` lists only the
/// repositories of the kept images, so nothing else is selected.
pub fn images_to_prune(present: &[String], keep: &[String]) -> Vec<String> {
    let mut out: Vec<String> = present
        .iter()
        .filter(|p| !keep.contains(p))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Remove release images other than `keep`, in the repositories of `keep`.
/// Best effort: failures are logged and never change an update's outcome.
pub(super) async fn prune_release_images<R: Runtime>(runtime: &R, keep: Vec<String>) {
    let mut repos: Vec<&str> = keep.iter().filter_map(|i| image_repository(i)).collect();
    repos.sort_unstable();
    repos.dedup();
    for repo in repos {
        let present = match runtime.list_images(repo).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(repository = repo, error = %e, "cannot list images to prune");
                continue;
            }
        };
        remove_release_images(runtime, images_to_prune(&present, &keep)).await;
    }
}

async fn remove_release_images<R: Runtime>(runtime: &R, images: Vec<String>) {
    for image in images {
        match runtime.remove_image(&image).await {
            Ok(()) => tracing::info!(image = %image, "pruned old release image"),
            Err(e) => tracing::warn!(image = %image, error = %e, "cannot prune image"),
        }
    }
}
