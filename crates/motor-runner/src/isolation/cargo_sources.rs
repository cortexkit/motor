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
    fn configured_cargo_roots_do_not_read_operator_cargo_home() {
        super::super::host_inputs::fixture::with(
            super::super::host_inputs::fixture::Inputs {
                environment: Default::default(),
                existing: Default::default(),
                directories: Default::default(),
            },
            || {
                let mut config = crate::Config::new("/runner/state", "test");
                config.cargo_home = Some("/scratch/cargo-home".into());
                config.image_cargo = "/scratch/image-cargo".into();
                assert_eq!(
                    config.cargo_home().unwrap(),
                    Path::new("/scratch/cargo-home")
                );
                for (layer, name) in layers(&config.cargo_home().unwrap(), &config.image_cargo)
                    .iter()
                    .zip(["registry", "git"])
                {
                    assert_eq!(layer.lower, Path::new("/scratch/image-cargo").join(name));
                    assert_eq!(layer.target, Path::new("/scratch/cargo-home").join(name));
                }
            },
        );
    }

    #[test]
    fn default_cargo_roots_keep_image_constant_and_environment_home() {
        super::super::host_inputs::fixture::with(
            super::super::host_inputs::fixture::Inputs {
                environment: [("CARGO_HOME", "/operator/cargo-home".into())].into(),
                existing: Default::default(),
                directories: Default::default(),
            },
            || {
                let config = crate::Config::new("/runner/state", "test");
                assert!(config.cargo_home.is_none());
                assert_eq!(config.image_cargo, Path::new(IMAGE_CARGO));
                assert_eq!(
                    config.cargo_home().unwrap(),
                    Path::new("/operator/cargo-home")
                );
            },
        );
    }

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
