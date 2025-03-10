/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Index for Cargo metadata, and various useful traversals.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error;
use std::fmt;

use anyhow::Result;
use serde::Deserialize;

use crate::buck::Name;
use crate::cargo::DepKind;
use crate::cargo::Manifest;
use crate::cargo::ManifestDep;
use crate::cargo::ManifestTarget;
use crate::cargo::Metadata;
use crate::cargo::Node;
use crate::cargo::NodeDep;
use crate::cargo::NodeDepKind;
use crate::cargo::PkgId;
use crate::cargo::TargetReq;
use crate::platform::PlatformExpr;
use crate::platform::PlatformPredicate;

/// Index for interesting things in Cargo metadata
pub struct Index<'meta> {
    /// Map a PkgId to the Manifest (package) with its details
    pkgid_to_pkg: HashMap<&'meta PkgId, &'meta Manifest>,
    /// Map a PkgId to a Node (ie all the details of a resolve dependency)
    pkgid_to_node: HashMap<&'meta PkgId, &'meta Node>,
    /// Represents the Cargo.toml itself
    pub root_pkg: &'meta Manifest,
    /// Set of packages from which at least one target is public.
    public_packages: BTreeSet<&'meta PkgId>,
    /// Set of public targets. These consist of:
    /// - root_pkg, if it is being made public (aka "real", and not just a pseudo package)
    /// - first-order dependencies of root_pkg, including artifact dependencies
    public_targets: BTreeMap<(&'meta PkgId, TargetReq<'meta>), Option<&'meta str>>,
}

/// Extra per-package metadata to be kept in sync with the package list
#[derive(Debug, Deserialize)]
pub struct ExtraMetadata {
    pub oncall: String, // oncall shortname for use as maintainer
}

// Cumulative errors in package metadata
#[derive(Debug, Clone)]
struct PackageMetaError {
    extra: BTreeSet<String>,
}

impl PackageMetaError {
    fn new() -> Self {
        PackageMetaError {
            extra: BTreeSet::new(),
        }
    }

    fn all_ok(&self) -> bool {
        self.extra.is_empty()
    }

    fn add_extra(&mut self, s: impl ToString) {
        self.extra.insert(s.to_string());
    }
}

impl error::Error for PackageMetaError {}

impl fmt::Display for PackageMetaError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if self.extra.is_empty() {
            write!(fmt, "Package Metadata: all OK")?;
        } else {
            write!(fmt, "Extra metadata for package(s):")?;
            for p in &self.extra {
                write!(fmt, " {}", p)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedDep<'meta> {
    pub package: &'meta Manifest,
    pub platform: Option<PlatformExpr>,
    pub rename: &'meta str,
    pub dep_kind: &'meta NodeDepKind,
}

impl<'meta> Index<'meta> {
    /// Construct an index for a set of Cargo metadata to allow convenient and efficient
    /// queries. The metadata represents a top level package and all its transitive
    /// dependencies.
    pub fn new(root_is_real: bool, metadata: &'meta Metadata) -> Index<'meta> {
        let pkgid_to_pkg: HashMap<_, _> = metadata.packages.iter().map(|m| (&m.id, m)).collect();

        let root_pkg: &Manifest = pkgid_to_pkg
            .get(&metadata.resolve.root.as_ref().expect("missing root pkg"))
            .expect("couldn't identify unambiguous top-level crate");

        let mut top_levels = HashSet::new();
        if root_is_real {
            top_levels.insert(&root_pkg.id);
        }

        let mut tmp = Index {
            pkgid_to_pkg,
            pkgid_to_node: metadata.resolve.nodes.iter().map(|n| (&n.id, n)).collect(),
            root_pkg,
            public_packages: BTreeSet::new(),
            public_targets: BTreeMap::new(),
        };

        // Keep an index of renamed crates, mapping from _ normalized name to actual name
        let dep_renamed: HashMap<String, &'meta str> = root_pkg
            .dependencies
            .iter()
            .filter_map(|dep| {
                let rename = dep.rename.as_deref()?;
                Some((rename.replace('-', "_"), rename))
            })
            .collect();

        // Compute public set, with pkgid mapped to rename if it has one. Public set is
        // anything in top_levels, or first-order dependencies of root_pkg.
        let public_targets = tmp
            .resolved_deps(tmp.root_pkg)
            .flat_map(|(rename, dep_kind, pkg)| {
                let target_req = dep_kind.target_req();
                let opt_rename = dep_renamed.get(rename).cloned();
                vec![((&pkg.id, target_req), opt_rename)]
            })
            .chain(top_levels.iter().flat_map(|pkgid| {
                [
                    ((*pkgid, TargetReq::Lib), None),
                    ((*pkgid, TargetReq::EveryBin), None),
                ]
            }))
            .collect::<BTreeMap<_, _>>();

        for (pkg, _kind) in public_targets.keys() {
            tmp.public_packages.insert(pkg);
        }

        Index {
            public_targets,
            ..tmp
        }
    }

    /// Test if a package is the root package
    pub fn is_root_package(&self, pkg: &Manifest) -> bool {
        self.root_pkg.id == pkg.id
    }

    /// Test if there is any target from the package which is public
    pub fn is_public_package(&self, pkg: &Manifest) -> bool {
        self.public_packages.contains(&pkg.id)
    }

    /// Test if a specific target from a package is public
    pub fn is_public_target(&self, pkg: &Manifest, target_req: TargetReq) -> bool {
        self.public_targets.contains_key(&(&pkg.id, target_req))
    }

    /// Returns the transitive closure of dependencies of public packages.
    pub fn all_packages(&self) -> impl Iterator<Item = &'meta Manifest> + '_ {
        self.pkgid_to_pkg.values().copied()
    }

    /// Return the private package rule name.
    pub fn private_rule_name(&self, pkg: &Manifest) -> Name {
        Name(match self.public_targets.get(&(&pkg.id, TargetReq::Lib)) {
            Some(None) | None => pkg.to_string(), // Full version info
            Some(Some(rename)) => format!("{}-{}", pkg, rename), // Rename
        })
    }

    /// Return the package public rule name.
    pub fn public_rule_name(&self, pkg: &'meta Manifest) -> Name {
        Name(match self.public_targets.get(&(&pkg.id, TargetReq::Lib)) {
            Some(None) | None => pkg.name.to_owned(), // Package name
            Some(&Some(rename)) => rename.to_owned(), // Rename
        })
    }

    pub fn get_extra_meta(&self) -> Result<HashMap<&'meta str, ExtraMetadata>> {
        // Package names borrowed from metadata
        let pubpkgs: HashSet<&'meta str> = self
            .root_pkg
            .dependencies
            .iter()
            .map(|dep| dep.name.as_str())
            .collect();
        let mut pkgerrs = PackageMetaError::new();

        let res = self.root_pkg.metadata.get("third-party").map_or_else(
            || Ok(HashMap::new()),
            |v| serde_json::from_value::<HashMap<String, ExtraMetadata>>(v.clone()),
        )?;

        let mut ret: HashMap<&'meta str, ExtraMetadata> = HashMap::new();
        for (name, val) in res {
            // remap names to borrowed from metadata, but also check to see if there's
            // extra metadata (metadata which references a pkg which doesn't exist)
            match pubpkgs.get(name.as_str()) {
                None => {
                    pkgerrs.add_extra(name);
                }
                Some(pkg) => {
                    ret.insert(pkg, val);
                }
            }
        }

        if pkgerrs.all_ok() {
            Ok(ret)
        } else {
            Err(From::from(pkgerrs))
        }
    }

    /// Return the set of features resolved for a particular package
    pub fn resolved_features(&self, pkg: &Manifest) -> impl Iterator<Item = &'meta str> {
        self.pkgid_to_node
            .get(&pkg.id)
            .unwrap()
            .features
            .iter()
            .map(String::as_str)
    }

    /// Return the resolved dependencies for a package
    /// This should generally be filtered by a target, but for the top-level we don't really care
    fn resolved_deps(
        &self,
        pkg: &Manifest,
    ) -> impl Iterator<Item = (&'meta str, &'meta NodeDepKind, &'meta Manifest)> + '_ {
        self.pkgid_to_node
            .get(&pkg.id)
            .unwrap()
            .deps
            .iter()
            .flat_map(
                |NodeDep {
                     pkg,
                     name,
                     dep_kinds,
                 }| {
                    dep_kinds.iter().map(|dep_kind| {
                        (
                            name.as_deref().or(dep_kind.extern_name.as_deref()).unwrap(),
                            dep_kind,
                            self.pkgid_to_pkg.get(pkg).copied().unwrap(),
                        )
                    })
                },
            )
    }

    /// Return the set of (unresolved) dependencies for a particular target.
    /// (Target must be the target for the given package.)
    fn deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
    ) -> impl Iterator<Item = &'meta ManifestDep> {
        assert!(pkg.targets.contains(tgt));

        pkg.dependencies.iter().filter(move |dep| match dep.kind {
            DepKind::Normal => {
                tgt.kind_lib() || tgt.kind_proc_macro() || tgt.kind_bin() || tgt.kind_cdylib()
            }
            DepKind::Dev => tgt.kind_bench() || tgt.kind_test() || tgt.kind_example(),
            DepKind::Build => tgt.kind_custom_build(),
        })
    }

    /// Return resolved dependencies for a target
    pub fn resolved_deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
    ) -> impl Iterator<Item = ResolvedDep<'meta>> + '_ {
        // Unresolved dependency names
        let mut deps = HashMap::new();

        // Dependencies can be repeated with different target predicates;
        // retain them all.
        for dep in self.deps_for_target(pkg, tgt) {
            deps.entry(dep.name.as_str())
                .or_insert_with(HashSet::new)
                .insert(dep);
        }

        // Resolved dependencies filtered by deps for target
        self.resolved_deps(pkg)
            .filter_map(move |(rename, dep_kind, dep)| {
                let mdeps = deps.get(dep.name.as_str())?;

                let mut platforms = vec![]; // empty = unconditional

                // If there are multiple manifestdeps then union all the
                // target predicates, where "unconditional" beats all.
                // (This is probably very over-engineered because all the times
                // this happens seem to be unconditional OR condition).
                for mdep in mdeps {
                    if let Some(plat) = &mdep.target {
                        match PlatformPredicate::parse(plat) {
                            Ok(pred) => platforms.push(pred),
                            Err(err) => {
                                log::error!("Failed to parse predicate for {}: {}", dep, err);
                                continue;
                            }
                        }
                    } else {
                        // No platform condition = unconditional
                        platforms = vec![];
                        break;
                    }
                }

                Some(ResolvedDep {
                    package: dep,
                    platform: match &*platforms {
                        [] => None,
                        [plat] => Some(format!("cfg({})", plat).into()),
                        _ => Some(format!("cfg({})", PlatformPredicate::Any(platforms)).into()),
                    },
                    rename,
                    dep_kind,
                })
            })
    }
}
