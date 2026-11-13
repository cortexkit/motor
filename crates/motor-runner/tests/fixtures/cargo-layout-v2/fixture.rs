use std::{fs, path::Path};

pub(crate) const V1: &str = include_str!("v1.txt");
pub(crate) const V2: &str = include_str!("v2.txt");

pub(crate) fn files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(self::files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

pub(crate) fn materialize(root: &Path, tree: &str) {
    let mut previous = None;
    for line in tree.lines() {
        if let Some(header) = line.strip_prefix("  dep-info: ") {
            fs::write(previous.as_ref().unwrap(), format!("{header}\n")).unwrap();
        } else {
            let path = root.join(line);
            if line.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "output").unwrap();
                previous = Some(path);
            }
        }
    }
}
