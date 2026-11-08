use std::path::{Path, PathBuf};

pub(crate) const IMAGE_CARGO: &str = "/usr/local/lib/motor/cargo";

pub(crate) struct Layer {
    pub(crate) lower: PathBuf,
    pub(crate) upper: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) target: PathBuf,
}

pub(crate) fn layers(home: &Path, image: &Path) -> [Layer; 2] {
    ["registry", "git"].map(|name| {
        let backing = home.with_file_name("cargo-source-layers").join(name);
        Layer {
            lower: image.join(name),
            upper: backing.join("upper"),
            work: backing.join("work"),
            target: home.join(name),
        }
    })
}

impl Layer {
    pub(crate) fn options(&self) -> String {
        format!("lowerdir={},upperdir={},workdir={},index=off,nosuid,nodev,metacopy=off,redirect_dir=nofollow",
            self.lower.display(), self.upper.display(), self.work.display())
    }
}

pub(crate) fn archive_lowers(home: &Path, image: Option<&Path>) -> Vec<PathBuf> {
    match image {
        Some(image) => vec![
            home.with_file_name("cargo-source-layers")
                .join("registry/upper/cache"),
            image.join("registry/cache"),
        ],
        None => vec![home.join("registry/cache")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_overlays_write_only_workspace_and_keep_the_image_as_lower() {
        let [registry, git] = layers(Path::new("/tenant/cargo-home"), Path::new(IMAGE_CARGO));
        for (layer, name) in [(&registry, "registry"), (&git, "git")] {
            assert_eq!(layer.lower, Path::new(IMAGE_CARGO).join(name));
            assert_eq!(
                layer.upper,
                Path::new("/tenant/cargo-source-layers")
                    .join(name)
                    .join("upper")
            );
            assert_eq!(
                layer.work,
                Path::new("/tenant/cargo-source-layers")
                    .join(name)
                    .join("work")
            );
            assert_eq!(layer.target, Path::new("/tenant/cargo-home").join(name));
        }
        assert_eq!(registry.options(), "lowerdir=/usr/local/lib/motor/cargo/registry,upperdir=/tenant/cargo-source-layers/registry/upper,workdir=/tenant/cargo-source-layers/registry/work,index=off,nosuid,nodev,metacopy=off,redirect_dir=nofollow");
    }

    #[test]
    fn net_archive_lowers_are_physical_layers_not_the_merged_overlay() {
        let home = Path::new("/tenant/cargo-home");
        assert_eq!(
            archive_lowers(home, Some(Path::new(IMAGE_CARGO))),
            [
                PathBuf::from("/tenant/cargo-source-layers/registry/upper/cache"),
                PathBuf::from("/usr/local/lib/motor/cargo/registry/cache"),
            ]
        );
        assert_eq!(
            archive_lowers(home, None),
            [PathBuf::from("/tenant/cargo-home/registry/cache")]
        );
    }
}
