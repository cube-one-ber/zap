//! Static .SRCINFO metadata. zap plans builds from it without sourcing the
//! PKGBUILD, then verifies after review that the two agree.
use crate::errors::{self, Kind, Result};
use crate::libalpm::Depend;
use crate::util::{append_unique, contains, dep_name, valid_name};

/// A `provides` entry and the split package that declares it; entries in
/// the pkgbase section apply to every package.
#[derive(Clone, Debug, PartialEq)]
struct Provide {
    owner: Option<String>,
    spec: String,
}

#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub base: String,
    pub version: String,
    pub names: Vec<String>,
    /// Runtime, make and check dependencies of every package in the base.
    pub deps: Vec<String>,
    /// The make and check dependencies alone: needed to build, not to run.
    pub build_deps: Vec<String>,
    pub provides: Vec<String>,
    scoped_provides: Vec<Provide>,
}

const DEPENDENCY_FIELDS: [&str; 4] = ["depends", "makedepends", "checkdepends", "provides"];

impl Metadata {
    pub fn parse(text: &str, arch: &str) -> Result<Metadata> {
        let mut result = Metadata::default();
        let (mut ver, mut rel, mut epoch) = ("", "", "");
        let mut current_package: Option<&str> = None;
        for line in text.split('\n') {
            let s = line.trim_matches([' ', '\r', '\t']);
            if s.is_empty() || s.starts_with('#') {
                continue;
            }
            let (key, value) = s.split_once('=').ok_or(Kind::InvalidSrcInfo)?;
            let (key, value) = (key.trim_matches([' ', '\t']), value.trim_matches([' ', '\t']));
            match key {
                "pkgbase" => result.base = value.into(),
                "pkgname" => {
                    if !valid_name(value) {
                        return errors::fail(Kind::InvalidSrcInfo);
                    }
                    append_unique(&mut result.names, value);
                    current_package = Some(value);
                }
                "pkgver" => ver = value,
                "pkgrel" => rel = value,
                "epoch" => epoch = value,
                _ => {
                    let Some(field) = DEPENDENCY_FIELDS.into_iter().find(|f| field_applies(key, f, arch)) else {
                        continue;
                    };
                    if !valid_name(dep_name(value)) {
                        return errors::fail(Kind::InvalidSrcInfo);
                    }
                    if field == "provides" {
                        append_unique(&mut result.provides, value);
                        result.scoped_provides.push(Provide { owner: current_package.map(Into::into), spec: value.into() });
                    } else {
                        append_unique(&mut result.deps, value);
                        if field != "depends" {
                            append_unique(&mut result.build_deps, value);
                        }
                    }
                }
            }
        }
        if !valid_name(&result.base) || result.names.is_empty() || ver.is_empty() || rel.is_empty() {
            return errors::fail(Kind::InvalidSrcInfo);
        }
        result.version = if !epoch.is_empty() && epoch != "0" { format!("{epoch}:{ver}-{rel}") } else { format!("{ver}-{rel}") };
        Ok(result)
    }

    /// The provides that apply to one package of the base.
    pub fn provides_for(&self, package: &str) -> Vec<String> {
        let mut list = Vec::new();
        for item in &self.scoped_provides {
            if item.owner.as_deref().is_none_or(|owner| owner == package) {
                append_unique(&mut list, &item.spec);
            }
        }
        list
    }

    /// Whether `package` from this base satisfies the dependency.
    pub fn matches(&self, dep: &str, package: &str) -> bool {
        satisfies(dep, package, &self.version, &self.provides_for(package))
    }

    /// The package of this base that satisfies the dependency, if any.
    pub fn provider(&self, dep: &str) -> Option<&str> {
        self.names.iter().find(|package| self.matches(dep, package)).map(String::as_str)
    }

    /// Whether two parses describe the same base, version, packages,
    /// dependencies and providers, in any order.
    pub fn equivalent(&self, other: &Metadata) -> bool {
        self.base == other.base
            && self.version == other.version
            && set_equal(&self.names, &other.names)
            && set_equal(&self.deps, &other.deps)
            && set_equal(&self.provides, &other.provides)
            && self.scoped_provides.len() == other.scoped_provides.len()
            && self.scoped_provides.iter().all(|item| other.scoped_provides.contains(item))
    }
}

/// Whether `key` is `field` itself or its variant for this architecture,
/// such as `depends_x86_64`.
fn field_applies(key: &str, field: &str, arch: &str) -> bool {
    key == field || key.strip_prefix(field).and_then(|rest| rest.strip_prefix('_')).is_some_and(|a| !a.is_empty() && a == arch)
}

fn set_equal(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|s| contains(b, s))
}

/// Whether a package with this name, version and provides list satisfies a
/// possibly versioned dependency. An unversioned provide never satisfies a
/// versioned dependency.
pub fn satisfies<S: AsRef<str>>(dep: &str, name: &str, version: &str, provides: &[S]) -> bool {
    let wanted = Depend::parse(dep);
    if dep_name(dep) == name && wanted.version_matches(version) {
        return true;
    }
    provides.iter().map(AsRef::as_ref).any(|provided| {
        if dep_name(dep) != dep_name(provided) {
            return false;
        }
        if !dep.contains(['<', '>', '=']) {
            return true;
        }
        provided.find('=').is_some_and(|at| wanted.version_matches(&provided[at + 1..]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srcinfo_supports_epochs_split_packages_and_architecture_specific_dependencies() {
        let meta = Metadata::parse("pkgbase = foo\npkgver = 2.0\npkgrel = 1\nepoch = 3\nmakedepends = cargo\ndepends_x86_64 = libfoo>=1\ndepends_aarch64 = wrong\npkgname = foo-cli\nprovides = foo=2.0\npkgname = foo-lib\ndepends = libc", "x86_64").unwrap();
        assert_eq!("3:2.0-1", meta.version);
        assert_eq!(2, meta.names.len());
        assert!(contains(&meta.deps, "libfoo>=1"));
        assert!(!contains(&meta.deps, "wrong"));
        assert!(satisfies("foo>=2", "foo-cli", &meta.version, &meta.provides));
        assert!(!satisfies("foo>=3", "foo-cli", &meta.version, &meta.provides));
        assert!(!satisfies("foo>=1", "bar", "5", &["foo"]));
    }

    #[test]
    fn virtual_providers_stay_attached_to_their_split_package() {
        let meta = Metadata::parse("pkgbase = suite\npkgver = 1\npkgrel = 1\npkgname = suite-a\nprovides = virtual-a=1\npkgname = suite-b\nprovides = virtual-b=1", "x86_64").unwrap();
        assert!(meta.matches("virtual-b>=1", "suite-b"));
        assert!(!meta.matches("virtual-b>=1", "suite-a"));
        assert_eq!(Some("suite-b"), meta.provider("virtual-b"));
        assert_eq!(None, meta.provider("unrelated"));
    }

    #[test]
    fn metadata_comparison_ignores_order_and_notices_every_substantive_difference() {
        let text = "pkgbase = suite\npkgver = 1\npkgrel = 1\ndepends = a\ndepends = b\npkgname = suite\nprovides = virtual=1\n";
        let original = Metadata::parse(text, "x86_64").unwrap();
        let reordered = Metadata::parse("pkgbase = suite\npkgver = 1\npkgrel = 1\ndepends = b\ndepends = a\npkgname = suite\nprovides = virtual=1\n", "x86_64").unwrap();
        assert!(original.equivalent(&reordered));
        for (from, to) in [("pkgver = 1", "pkgver = 2"), ("depends = b", "depends = c"), ("virtual=1", "virtual=2"), ("pkgname = suite", "pkgname = other")] {
            let altered = Metadata::parse(&text.replace(from, to), "x86_64").unwrap();
            assert!(!original.equivalent(&altered), "{from} -> {to}");
        }
    }
}
