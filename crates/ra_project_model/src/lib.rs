mod cargo_workspace;
mod json_project;
mod sysroot;

use std::{
    error::Error,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
};

use rustc_hash::FxHashMap;

use ra_db::{CrateGraph, Edition, FileId};

use serde_json::from_reader;

use relative_path::RelativePath;

pub use crate::{
    cargo_workspace::{CargoWorkspace, Package, Target, TargetKind},
    json_project::JsonProject,
    sysroot::Sysroot,
};

// FIXME use proper error enum
pub type Result<T> = ::std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone)]
pub enum ProjectWorkspace {
    /// Project workspace was discovered by running `cargo metadata` and `rustc --print sysroot`.
    Cargo { cargo: CargoWorkspace, sysroot: Sysroot },
    /// Project workspace was manually specified using a `rust-project.json` file.
    Json { project: JsonProject },
}

/// `ProjectRoot` describes a workspace root folder.
/// Which may be an external dependency, or a member of
/// the current workspace.
#[derive(Clone)]
pub struct ProjectRoot {
    /// Path to the root folder
    path: PathBuf,
    /// Is a member of the current workspace
    is_member: bool,
}

impl ProjectRoot {
    pub fn new(path: PathBuf, is_member: bool) -> ProjectRoot {
        ProjectRoot { path, is_member }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn is_member(&self) -> bool {
        self.is_member
    }

    pub fn include_dir(&self, dir_path: &RelativePath) -> bool {
        const COMMON_IGNORED_DIRS: &[&str] = &[
            "node_modules",
            "target",
            ".git",
            // 0 .rs files
            "obj",
            "build",
            "ci",
            "jemalloc",
            "llvm-emscripten",
            "llvm",
            // 3 renderscript.rs files that are not rust
            "llvm-project",
            // 20 .rs files used for formatting the book
            "docs",
            // A bunch of llvm-related things in src/tools/
            "lld",
            "lldb",
            "clang",
        ];
        const EXTERNAL_IGNORED_DIRS: &[&str] = &["examples", "tests", "benches"];

        let is_ignored = if self.is_member {
            dir_path.components().any(|c| COMMON_IGNORED_DIRS.contains(&c.as_str()))
        } else {
            dir_path.components().any(|c| {
                let path = c.as_str();
                COMMON_IGNORED_DIRS.contains(&path) || EXTERNAL_IGNORED_DIRS.contains(&path)
            })
        };

        let hidden = dir_path.components().any(|c| c.as_str().starts_with('.'));

        !is_ignored && !hidden
    }

    pub fn include_file(&self, file_path: &RelativePath) -> bool {
        file_path.extension() == Some("rs")
    }
}

impl ProjectWorkspace {
    pub fn discover(path: &Path) -> Result<ProjectWorkspace> {
        match find_rust_project_json(path) {
            Some(json_path) => {
                let file = File::open(json_path)?;
                let reader = BufReader::new(file);
                Ok(ProjectWorkspace::Json { project: from_reader(reader)? })
            }
            None => {
                let cargo_toml = find_cargo_toml(path)?;
                Ok(ProjectWorkspace::Cargo {
                    cargo: CargoWorkspace::from_cargo_metadata(&cargo_toml)?,
                    sysroot: Sysroot::discover(&cargo_toml)?,
                })
            }
        }
    }

    /// Returns the roots for the current ProjectWorkspace
    /// The return type contains the path and whether or not
    /// the root is a member of the current workspace
    pub fn to_roots(&self) -> Vec<ProjectRoot> {
        match self {
            ProjectWorkspace::Json { project } => {
                let mut roots = Vec::with_capacity(project.roots.len());
                for root in &project.roots {
                    roots.push(ProjectRoot::new(root.path.clone(), true));
                }
                roots
            }
            ProjectWorkspace::Cargo { cargo, sysroot } => {
                let mut roots =
                    Vec::with_capacity(cargo.packages().count() + sysroot.crates().count());
                for pkg in cargo.packages() {
                    let root = pkg.root(&cargo).to_path_buf();
                    let member = pkg.is_member(&cargo);
                    roots.push(ProjectRoot::new(root, member));
                }
                for krate in sysroot.crates() {
                    roots.push(ProjectRoot::new(krate.root_dir(&sysroot).to_path_buf(), false))
                }
                roots
            }
        }
    }

    pub fn count(&self) -> usize {
        match self {
            ProjectWorkspace::Json { project } => project.crates.len(),
            ProjectWorkspace::Cargo { cargo, .. } => cargo.packages().count(),
        }
    }

    pub fn to_crate_graph(&self, load: &mut dyn FnMut(&Path) -> Option<FileId>) -> CrateGraph {
        let mut crate_graph = CrateGraph::default();
        match self {
            ProjectWorkspace::Json { project } => {
                let mut crates = FxHashMap::default();
                for (id, krate) in project.crates.iter().enumerate() {
                    let crate_id = json_project::CrateId(id);
                    if let Some(file_id) = load(&krate.root_module) {
                        let edition = match krate.edition {
                            json_project::Edition::Edition2015 => Edition::Edition2015,
                            json_project::Edition::Edition2018 => Edition::Edition2018,
                        };
                        crates.insert(crate_id, crate_graph.add_crate_root(file_id, edition));
                    }
                }

                for (id, krate) in project.crates.iter().enumerate() {
                    for dep in &krate.deps {
                        let from_crate_id = json_project::CrateId(id);
                        let to_crate_id = dep.krate;
                        if let (Some(&from), Some(&to)) =
                            (crates.get(&from_crate_id), crates.get(&to_crate_id))
                        {
                            if let Err(_) = crate_graph.add_dep(from, dep.name.clone().into(), to) {
                                log::error!(
                                    "cyclic dependency {:?} -> {:?}",
                                    from_crate_id,
                                    to_crate_id
                                );
                            }
                        }
                    }
                }
            }
            ProjectWorkspace::Cargo { cargo, sysroot } => {
                let mut sysroot_crates = FxHashMap::default();
                for krate in sysroot.crates() {
                    if let Some(file_id) = load(krate.root(&sysroot)) {
                        sysroot_crates.insert(
                            krate,
                            crate_graph.add_crate_root(file_id, Edition::Edition2015),
                        );
                    }
                }
                for from in sysroot.crates() {
                    for to in from.deps(&sysroot) {
                        let name = to.name(&sysroot);
                        if let (Some(&from), Some(&to)) =
                            (sysroot_crates.get(&from), sysroot_crates.get(&to))
                        {
                            if let Err(_) = crate_graph.add_dep(from, name.into(), to) {
                                log::error!("cyclic dependency between sysroot crates")
                            }
                        }
                    }
                }

                let libstd = sysroot.std().and_then(|it| sysroot_crates.get(&it).copied());

                let mut pkg_to_lib_crate = FxHashMap::default();
                let mut pkg_crates = FxHashMap::default();
                // Next, create crates for each package, target pair
                for pkg in cargo.packages() {
                    let mut lib_tgt = None;
                    for tgt in pkg.targets(&cargo) {
                        let root = tgt.root(&cargo);
                        if let Some(file_id) = load(root) {
                            let edition = pkg.edition(&cargo);
                            let crate_id = crate_graph.add_crate_root(file_id, edition);
                            if tgt.kind(&cargo) == TargetKind::Lib {
                                lib_tgt = Some(crate_id);
                                pkg_to_lib_crate.insert(pkg, crate_id);
                            }
                            pkg_crates.entry(pkg).or_insert_with(Vec::new).push(crate_id);
                        }
                    }

                    // Set deps to the std and to the lib target of the current package
                    for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                        if let Some(to) = lib_tgt {
                            if to != from {
                                if let Err(_) =
                                    crate_graph.add_dep(from, pkg.name(&cargo).into(), to)
                                {
                                    log::error!(
                                        "cyclic dependency between targets of {}",
                                        pkg.name(&cargo)
                                    )
                                }
                            }
                        }
                        if let Some(std) = libstd {
                            if let Err(_) = crate_graph.add_dep(from, "std".into(), std) {
                                log::error!("cyclic dependency on std for {}", pkg.name(&cargo))
                            }
                        }
                    }
                }

                // Now add a dep ednge from all targets of upstream to the lib
                // target of downstream.
                for pkg in cargo.packages() {
                    for dep in pkg.dependencies(&cargo) {
                        if let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) {
                            for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                                if let Err(_) =
                                    crate_graph.add_dep(from, dep.name.clone().into(), to)
                                {
                                    log::error!(
                                        "cyclic dependency {} -> {}",
                                        pkg.name(&cargo),
                                        dep.pkg.name(&cargo)
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
        crate_graph
    }

    pub fn workspace_root_for(&self, path: &Path) -> Option<&Path> {
        match self {
            ProjectWorkspace::Cargo { cargo, .. } => {
                Some(cargo.workspace_root.as_ref()).filter(|root| path.starts_with(root))
            }
            ProjectWorkspace::Json { project: JsonProject { roots, .. } } => roots
                .iter()
                .find(|root| path.starts_with(&root.path))
                .map(|root| root.path.as_ref()),
        }
    }
}

fn find_rust_project_json(path: &Path) -> Option<PathBuf> {
    if path.ends_with("rust-project.json") {
        return Some(path.to_path_buf());
    }

    let mut curr = Some(path);
    while let Some(path) = curr {
        let candidate = path.join("rust-project.json");
        if candidate.exists() {
            return Some(candidate);
        }
        curr = path.parent();
    }

    None
}

fn find_cargo_toml(path: &Path) -> Result<PathBuf> {
    if path.ends_with("Cargo.toml") {
        return Ok(path.to_path_buf());
    }
    let mut curr = Some(path);
    while let Some(path) = curr {
        let candidate = path.join("Cargo.toml");
        if candidate.exists() {
            return Ok(candidate);
        }
        curr = path.parent();
    }
    Err(format!("can't find Cargo.toml at {}", path.display()))?
}
