//! Built package archives. makepkg predicts the paths it will write; zap
//! trusts none of that and identifies each archive by its own metadata.
use crate::alpm::{self, Alpm};
use crate::errors::{self, Kind, Result};
use crate::libalpm::archive;
use crate::transaction::{self, Target};
use crate::util::{contains, valid_name};
use std::collections::HashMap;

pub struct Options<'a> {
    pub base: &'a str,
    pub dir: &'a str,
    /// The packages of this base that the plan needs, in installation order.
    pub wanted: &'a [String],
    pub explicit_names: &'a [String],
    /// Keep the reason an already installed package has (upgrades).
    pub preserve_reasons: bool,
}

pub struct Collected {
    pub targets: Vec<Target>,
    pub versions: Vec<String>,
}

/// makepkg predicts package paths, including optional debug archives. Only
/// the automatic debug output may be absent. Validate actual metadata and
/// exact required-name coverage before returning targets in the requested
/// order.
pub fn collect(db: &Alpm, options: &Options, paths: &str) -> Result<Collected> {
    if options.wanted.is_empty() {
        return errors::fail(Kind::NoTargets);
    }
    for (i, name) in options.wanted.iter().enumerate() {
        if !valid_name(name) {
            return Err(errors::with(Kind::InvalidPackageName, name));
        }
        if contains(&options.wanted[..i], name) {
            return Err(errors::with(Kind::DuplicateTarget, name));
        }
    }
    let debug_name = format!("{}-debug", options.base);
    let debug_prefix = format!("{debug_name}-");
    let mut found: HashMap<String, (Target, String)> = HashMap::new();
    for line in paths.split('\n').filter(|l| !l.is_empty()) {
        let path = crate::util::resolve(options.dir, line);
        if let Err(e) = std::fs::symlink_metadata(&path) {
            // Only the automatic debug package may be predicted but absent.
            let filename = path.rsplit('/').next().unwrap_or("");
            let optional_debug = !contains(options.wanted, &debug_name) && filename.starts_with(&debug_prefix) && filename.contains(".pkg.tar");
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            if missing && optional_debug {
                continue;
            }
            return Err(if missing { errors::with(Kind::MissingBuildArtifacts, &path) } else { errors::with(e, &path) });
        }
        let digest = transaction::hash_file(&path)?;
        let package = archive::load(&path, true).map_err(|_| errors::with(Kind::InvalidBuildArchive, &path))?;
        let name = package.name.as_str();
        if !contains(options.wanted, name) {
            continue;
        }
        if found.contains_key(name) {
            return Err(errors::with(Kind::DuplicateBuildArtifact, format!("{name} ({path})")));
        }
        let declared_base = package.info().base.as_str();
        let base = if declared_base.is_empty() { name } else { declared_base };
        if base != options.base {
            return Err(errors::with(Kind::ArchiveBaseMismatch, format!("{path} belongs to {base}, expected {}", options.base)));
        }
        let requested = contains(options.explicit_names, name);
        let dependency = match options.preserve_reasons.then(|| db.local(name)).flatten() {
            Some(old) => !alpm::explicit(old),
            None => !requested,
        };
        let target = Target { archive: Some(path.clone()), sha256: Some(digest), dependency, ..Target::named(name) };
        found.insert(name.to_string(), (target, package.version.clone()));
    }
    let missing: Vec<&str> = options.wanted.iter().filter(|name| !found.contains_key(*name)).map(String::as_str).collect();
    if !missing.is_empty() {
        return Err(errors::with(Kind::MissingBuildArtifacts, format!("{} did not produce {}", options.base, missing.join(", "))));
    }
    let (targets, versions) = options.wanted.iter().map(|name| found.remove(name).unwrap()).unzip();
    Ok(Collected { targets, versions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, TempDir};

    struct Fixture {
        tmp: TempDir,
        db: Alpm,
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = TempDir::new();
            let db = testing::alpm(tmp.path());
            Fixture { tmp, db }
        }
        fn root(&self) -> &str {
            self.tmp.str()
        }
        fn archive(&self, name: &str, base: &str) -> String {
            testing::package_fixture(self.tmp.path(), name, &format!("pkgbase = {base}\n"))
        }
        fn collect(&self, wanted: &[&str], paths: &str) -> Result<Collected> {
            let (wanted, explicit) = (names(wanted), names(&["alpha"]));
            let options = Options { base: "suite", dir: self.root(), wanted: &wanted, explicit_names: &explicit, preserve_reasons: false };
            collect(&self.db, &options, paths)
        }
        fn fails(&self, wanted: &[&str], paths: &str) -> Kind {
            self.collect(wanted, paths).err().expect("collect should fail").kind
        }
    }

    #[test]
    fn artifact_selection_uses_exact_names_requested_order_and_optional_debug_predictions() {
        let f = Fixture::new();
        let (alpha, beta) = (f.archive("alpha", "suite"), f.archive("beta", "suite"));
        let paths = format!("{beta}\n{alpha}\n{}/suite-debug-1.0-1-any.pkg.tar.zst\n", f.root());
        let result = f.collect(&["alpha", "beta"], &paths).unwrap();
        assert_eq!(("alpha", "beta"), (result.targets[0].name.as_str(), result.targets[1].name.as_str()));
        assert!(!result.targets[0].dependency);
        assert!(result.targets[1].dependency);
        assert_eq!("1.0-1", result.versions[0]);
        assert_eq!(64, result.targets[0].sha256.as_ref().unwrap().len());
    }

    #[test]
    fn missing_required_packages_and_non_debug_predicted_files_fail() {
        let f = Fixture::new();
        let alpha = f.archive("alpha", "suite");
        assert_eq!(Kind::MissingBuildArtifacts, f.fails(&["alpha", "beta"], &alpha));
        assert_eq!(Kind::MissingBuildArtifacts, f.fails(&["beta"], &format!("{}/beta-1.0-1-any.pkg.tar", f.root())));
        assert_eq!(Kind::MissingBuildArtifacts, f.fails(&["suite-debug"], &format!("{}/suite-debug-1.0-1-any.pkg.tar", f.root())));
    }

    #[test]
    fn duplicate_metadata_cannot_conceal_a_missing_split_package() {
        let f = Fixture::new();
        let alpha = f.archive("alpha", "suite");
        assert_eq!(Kind::DuplicateBuildArtifact, f.fails(&["alpha", "beta"], &format!("{alpha}\n{alpha}")));
        std::fs::copy(&alpha, f.tmp.path().join("alias.pkg.tar")).unwrap();
        assert_eq!(Kind::DuplicateBuildArtifact, f.fails(&["alpha", "beta"], &format!("{alpha}\nalias.pkg.tar")));
    }

    #[test]
    fn symlinks_and_special_files_are_rejected_including_optional_debug_outputs() {
        let f = Fixture::new();
        let alpha = f.archive("alpha", "suite");
        std::os::unix::fs::symlink(&alpha, f.tmp.path().join("suite-debug-1.0-1-any.pkg.tar")).unwrap();
        assert_eq!(Kind::UnsafeArchive, f.fails(&["alpha"], &format!("{alpha}\nsuite-debug-1.0-1-any.pkg.tar")));
        let fifo = format!("{}/pipe.pkg.tar", f.root());
        assert_eq!(0, unsafe { libc::mkfifo(crate::util::cstr(&fifo).as_ptr(), 0o600) });
        assert_eq!(Kind::UnsafeArchive, f.fails(&["alpha"], &fifo));
    }

    #[test]
    fn corrupt_archives_and_mismatched_package_bases_fail_before_returning_targets() {
        let f = Fixture::new();
        std::fs::write(f.tmp.path().join("corrupt.pkg.tar"), "not a package archive").unwrap();
        assert_eq!(Kind::InvalidBuildArchive, f.fails(&["alpha"], "corrupt.pkg.tar"));
        let wrong = f.archive("alpha", "different-base");
        assert_eq!(Kind::ArchiveBaseMismatch, f.fails(&["alpha"], &wrong));
    }

    #[test]
    fn existing_debug_packages_are_selected_only_when_explicitly_required() {
        let f = Fixture::new();
        let paths = format!("{}\n{}", f.archive("alpha", "suite"), f.archive("suite-debug", "suite"));
        assert_eq!(1, f.collect(&["alpha"], &paths).unwrap().targets.len());
        assert_eq!(2, f.collect(&["alpha", "suite-debug"], &paths).unwrap().targets.len());
    }

    #[test]
    fn artifact_paths_preserve_spaces_and_resolve_relative_package_destinations() {
        let f = Fixture::new();
        let alpha = f.archive("alpha", "suite");
        std::fs::create_dir(f.tmp.path().join("package output")).unwrap();
        std::fs::copy(&alpha, f.tmp.path().join("package output/alpha.pkg.tar")).unwrap();
        let result = f.collect(&["alpha"], "./package output/alpha.pkg.tar\n").unwrap();
        let archive = result.targets[0].archive.as_deref().unwrap();
        assert_eq!("alpha", result.targets[0].name);
        assert!(archive.starts_with('/') && archive.contains("package output"));
    }

    #[test]
    fn duplicate_or_invalid_required_names_cannot_produce_ambiguous_owned_results() {
        let f = Fixture::new();
        assert_eq!(Kind::DuplicateTarget, f.fails(&["alpha", "alpha"], ""));
        assert_eq!(Kind::InvalidPackageName, f.fails(&["../alpha"], ""));
        assert_eq!(Kind::NoTargets, f.fails(&[], ""));
    }
}
