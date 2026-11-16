//! Dependency expressions and the checks built on them, as libalpm's deps.c.
use super::package::Pkg;
use super::version::vercmp;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepMod {
    Any,
    Eq,
    Ge,
    Le,
    Gt,
    Lt,
}

/// A dependency such as `foo>=1.2: why it is needed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Depend {
    pub name: String,
    pub version: Option<String>,
    pub desc: Option<String>,
    pub modifier: DepMod,
}

impl Depend {
    /// Parse like `alpm_dep_from_string`.
    pub fn parse(depstring: &str) -> Depend {
        // The space in ": " avoids matching an epoch.
        let (spec, desc) = match depstring.find(": ") {
            Some(at) => (&depstring[..at], Some(depstring[at + 2..].to_string())),
            None => (depstring, None),
        };
        let (name_end, modifier, version_start) = if let Some(at) = spec.find('<') {
            if spec.as_bytes().get(at + 1) == Some(&b'=') {
                (at, DepMod::Le, at + 2)
            } else {
                (at, DepMod::Lt, at + 1)
            }
        } else if let Some(at) = spec.find('>') {
            if spec.as_bytes().get(at + 1) == Some(&b'=') {
                (at, DepMod::Ge, at + 2)
            } else {
                (at, DepMod::Gt, at + 1)
            }
        } else if let Some(at) = spec.find('=') {
            (at, DepMod::Eq, at + 1)
        } else {
            (spec.len(), DepMod::Any, spec.len())
        };
        let version = if modifier == DepMod::Any { None } else { Some(spec[version_start..].to_string()) };
        Depend { name: spec[..name_end].to_string(), version, desc, modifier }
    }

    fn operator(&self) -> &'static str {
        match self.modifier {
            DepMod::Any => "",
            DepMod::Ge => ">=",
            DepMod::Le => "<=",
            DepMod::Eq => "=",
            DepMod::Lt => "<",
            DepMod::Gt => ">",
        }
    }

    /// Whether `version` meets this dependency's version constraint.
    pub fn version_matches(&self, version: &str) -> bool {
        dep_vercmp(version, self.modifier, self.version.as_deref().unwrap_or(""))
    }

    /// Whether a package satisfies this dependency by name or provision.
    pub fn satisfied_by(&self, pkg: &Pkg) -> bool {
        self.literal(pkg) || self.provided_by(&pkg.info().provides)
    }

    /// Whether a package satisfies this dependency by its own name.
    pub fn literal(&self, pkg: &Pkg) -> bool {
        pkg.name == self.name && self.version_matches(&pkg.version)
    }

    /// Whether any provision satisfies this dependency.
    pub fn provided_by(&self, provisions: &[Depend]) -> bool {
        provisions.iter().any(|p| {
            if self.modifier == DepMod::Any {
                p.name == self.name
            } else if p.modifier == DepMod::Eq {
                p.name == self.name && dep_vercmp(p.version.as_deref().unwrap_or(""), self.modifier, self.version.as_deref().unwrap_or(""))
            } else {
                false
            }
        })
    }
}

/// `alpm_dep_compute_string`.
impl fmt::Display for Depend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        f.write_str(self.operator())?;
        if self.modifier != DepMod::Any {
            if let Some(v) = &self.version {
                f.write_str(v)?;
            }
        }
        if let Some(desc) = &self.desc {
            f.write_str(": ")?;
            f.write_str(desc)?;
        }
        Ok(())
    }
}

fn dep_vercmp(version1: &str, modifier: DepMod, version2: &str) -> bool {
    if modifier == DepMod::Any {
        return true;
    }
    let cmp = vercmp(version1, version2);
    match modifier {
        DepMod::Eq => cmp == 0,
        DepMod::Ge => cmp >= 0,
        DepMod::Le => cmp <= 0,
        DepMod::Lt => cmp < 0,
        DepMod::Gt => cmp > 0,
        DepMod::Any => true,
    }
}

/// The first package that satisfies `dep`, as `find_dep_satisfier`.
pub fn find_satisfier<'a, I>(pkgs: I, dep: &Depend) -> Option<&'a Arc<Pkg>>
where
    I: IntoIterator<Item = &'a Arc<Pkg>>,
{
    pkgs.into_iter().find(|p| dep.satisfied_by(p))
}

/// A dependency a transaction would leave unsatisfied.
#[derive(Clone, Debug)]
pub struct DepMissing {
    pub target: String,
    pub depend: Depend,
    /// The package whose removal or upgrade breaks the dependency.
    pub causing: Option<String>,
}

/// `alpm_checkdeps`: dependencies of `upgrade` left unsatisfied, and with
/// `reversedeps`, installed dependencies broken by removing `remove` or
/// replacing it by `upgrade`.
pub fn checkdeps(
    pkglist: &[Arc<Pkg>],
    remove: &[Arc<Pkg>],
    upgrade: &[Arc<Pkg>],
    reversedeps: bool,
) -> Vec<DepMissing> {
    checkdeps_with(pkglist, remove, upgrade, reversedeps, &[], false)
}

/// `alpm_checkdeps` with the handle's assumed-installed provisions and the
/// transaction's NODEPVERSION flag.
pub fn checkdeps_with(
    pkglist: &[Arc<Pkg>],
    remove: &[Arc<Pkg>],
    upgrade: &[Arc<Pkg>],
    reversedeps: bool,
    assumed: &[Depend],
    nodepversion: bool,
) -> Vec<DepMissing> {
    let touched = |p: &Pkg| remove.iter().chain(upgrade).any(|q| q.name == p.name);
    let (modified, dblist): (Vec<Arc<Pkg>>, Vec<Arc<Pkg>>) = pkglist.iter().cloned().partition(|p| touched(p));
    let relax = |depend: &Depend| -> Depend {
        let mut depend = depend.clone();
        if nodepversion {
            depend.modifier = DepMod::Any;
        }
        depend
    };
    let mut missing = Vec::new();
    for tp in upgrade {
        for original in &tp.info().depends {
            let depend = relax(original);
            if find_satisfier(upgrade, &depend).is_none()
                && find_satisfier(&dblist, &depend).is_none()
                && !depend.provided_by(assumed)
            {
                missing.push(DepMissing { target: tp.name.clone(), depend, causing: None });
            }
        }
    }
    if reversedeps {
        for lp in &dblist {
            for original in &lp.info().depends {
                let depend = relax(original);
                let Some(causing) = find_satisfier(&modified, &depend) else { continue };
                if find_satisfier(upgrade, &depend).is_none()
                    && find_satisfier(&dblist, &depend).is_none()
                    && !depend.provided_by(assumed)
                {
                    missing.push(DepMissing {
                        target: lp.name.clone(),
                        depend,
                        causing: Some(causing.name.clone()),
                    });
                }
            }
        }
    }
    missing
}

/// `_alpm_pkg_depends_on`: whether `pkg1` depends on `pkg2`.
pub fn depends_on(pkg1: &Pkg, pkg2: &Pkg) -> bool {
    pkg1.info().depends.iter().any(|d| d.satisfied_by(pkg2))
}

/// `_alpm_sortbydeps`: order `targets` so dependencies come first (last
/// with `reverse`). Installed packages outside `targets` and `ignore` count
/// for indirect dependencies.
pub fn sortbydeps(local: &[Arc<Pkg>], targets: &[Arc<Pkg>], ignore: &[Arc<Pkg>], reverse: bool) -> Vec<Arc<Pkg>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Unprocessed,
        Processing,
        Processed,
    }
    struct Vertex {
        pkg: Arc<Pkg>,
        children: Vec<usize>,
        next_child: usize,
        parent: Option<usize>,
        mark: Mark,
    }
    if targets.is_empty() {
        return Vec::new();
    }
    let named = |list: &[Arc<Pkg>], p: &Pkg| list.iter().any(|q| q.name == p.name);
    let mut localpkgs: Vec<Arc<Pkg>> =
        local.iter().filter(|p| !named(targets, p) && !named(ignore, p)).cloned().collect();
    let new = |pkg: Arc<Pkg>| Vertex { pkg, children: Vec::new(), next_child: 0, parent: None, mark: Mark::Unprocessed };
    let mut vertices: Vec<Vertex> = targets.iter().cloned().map(new).collect();
    // Edges; installed packages join the graph lazily, only when depended on.
    let mut i = 0;
    while i < vertices.len() {
        let p_i = vertices[i].pkg.clone();
        let mut children: Vec<usize> =
            (0..vertices.len()).filter(|&j| depends_on(&p_i, &vertices[j].pkg)).collect();
        let mut k = 0;
        while k < localpkgs.len() {
            if depends_on(&p_i, &localpkgs[k]) {
                let pkg = localpkgs.remove(k);
                vertices.push(new(pkg));
                children.push(vertices.len() - 1);
            } else {
                k += 1;
            }
        }
        vertices[i].children = children;
        i += 1;
    }
    let is_target = |p: &Arc<Pkg>| targets.iter().any(|t| Arc::ptr_eq(t, p));
    let mut sorted: Vec<Arc<Pkg>> = Vec::new();
    let mut top = 0;
    let mut v = 0;
    loop {
        vertices[v].mark = Mark::Processing;
        let mut switched = false;
        while vertices[v].next_child < vertices[v].children.len() && !switched {
            let child = vertices[v].children[vertices[v].next_child];
            vertices[v].next_child += 1;
            match vertices[child].mark {
                Mark::Unprocessed => {
                    switched = true;
                    vertices[child].parent = Some(v);
                    v = child;
                }
                Mark::Processing => {
                    if is_target(&vertices[child].pkg) {
                        // Find the nearest ancestor in the transaction.
                        let mut ancestor = Some(v);
                        while let Some(a) = ancestor {
                            if is_target(&vertices[a].pkg) {
                                break;
                            }
                            ancestor = vertices[a].parent;
                        }
                        if let Some(a) = ancestor.filter(|&a| a != child) {
                            let (a, c) = (&vertices[a].pkg.name, &vertices[child].pkg.name);
                            super::log::debug("dependency cycle detected:\n");
                            if reverse {
                                super::log::debug(&format!("{a} will be removed after its {c} dependency\n"));
                            } else {
                                super::log::debug(&format!("{a} will be installed before its {c} dependency\n"));
                            }
                        }
                    }
                }
                Mark::Processed => {}
            }
        }
        if !switched {
            if is_target(&vertices[v].pkg) {
                sorted.push(vertices[v].pkg.clone());
            }
            vertices[v].mark = Mark::Processed;
            match vertices[v].parent {
                Some(parent) => v = parent,
                None => {
                    top += 1;
                    while top < vertices.len() && vertices[top].mark != Mark::Unprocessed {
                        top += 1;
                    }
                    if top >= vertices.len() {
                        break;
                    }
                    v = top;
                }
            }
        }
    }
    if reverse {
        sorted.reverse();
    }
    sorted
}

/// `_alpm_recursedeps`: add to `targets` the installed dependencies that
/// only they need. Explicitly installed ones only with `include_explicit`.
pub fn recursedeps(local: &[Arc<Pkg>], targets: &mut Vec<Arc<Pkg>>, include_explicit: bool) {
    fn select(from: &mut Vec<Arc<Pkg>>, to: &mut Vec<Arc<Pkg>>, pkg: &Pkg, explicit: bool) {
        if pkg.info().depends.is_empty() {
            return;
        }
        let mut i = 0;
        while i < from.len() {
            let deppkg = &from[i];
            if (explicit || deppkg.info().reason == super::package::Reason::Depend) && depends_on(pkg, deppkg) {
                to.push(from.remove(i));
            } else {
                i += 1;
            }
        }
    }
    let mut keep: Vec<Arc<Pkg>> = local.iter().filter(|p| !targets.iter().any(|t| t.name == p.name)).cloned().collect();
    let mut rem: Vec<Arc<Pkg>> = Vec::new();
    for t in targets.iter() {
        select(&mut keep, &mut rem, t, include_explicit);
    }
    let mut i = 0;
    while i < rem.len() {
        let pkg = rem[i].clone();
        select(&mut keep, &mut rem, &pkg, include_explicit);
        i += 1;
    }
    // Anything still needed by a kept package stays.
    let mut i = 0;
    while i < keep.len() && !rem.is_empty() {
        let pkg = keep[i].clone();
        select(&mut rem, &mut keep, &pkg, true);
        i += 1;
    }
    for pkg in rem {
        super::log::debug(&format!("adding '{}' to the targets\n", pkg.name));
        targets.push(pkg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_strings_round_trip_like_libalpm() {
        for (input, name, modifier, version, desc) in [
            ("foo", "foo", DepMod::Any, None, None),
            ("foo>=2:1.0-1", "foo", DepMod::Ge, Some("2:1.0-1"), None),
            ("foo<=1", "foo", DepMod::Le, Some("1"), None),
            ("foo<1", "foo", DepMod::Lt, Some("1"), None),
            ("foo>1", "foo", DepMod::Gt, Some("1"), None),
            ("foo=1", "foo", DepMod::Eq, Some("1"), None),
            ("sudo: for elevation", "sudo", DepMod::Any, None, Some("for elevation")),
            ("bar=1:2: why", "bar", DepMod::Eq, Some("1:2"), Some("why")),
        ] {
            let dep = Depend::parse(input);
            assert_eq!((name, modifier), (dep.name.as_str(), dep.modifier), "{input}");
            assert_eq!(version, dep.version.as_deref(), "{input}");
            assert_eq!(desc, dep.desc.as_deref(), "{input}");
            assert_eq!(input, dep.to_string());
        }
    }

    #[test]
    fn provisions_satisfy_versioned_dependencies_only_with_an_exact_version() {
        let provides = [Depend::parse("libfoo.so=2-64"), Depend::parse("bar")];
        assert!(Depend::parse("libfoo.so").provided_by(&provides));
        assert!(Depend::parse("libfoo.so>=2").provided_by(&provides));
        assert!(!Depend::parse("libfoo.so>2-64").provided_by(&provides));
        assert!(Depend::parse("bar").provided_by(&provides));
        assert!(!Depend::parse("bar>=1").provided_by(&provides));
    }
}
