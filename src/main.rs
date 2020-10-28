#[macro_use] extern crate log as logcrate;
use logcrate::debug;

use std::path::Path;
use std::path::PathBuf;
use std::collections::BTreeMap;
use anyhow::Result;
use anyhow::Error;
use walkdir::WalkDir;
use indicatif::*;
use tokio::stream::StreamExt;

mod cli;
mod job;
mod util;
mod log;
mod package;
mod phase;
mod config;
mod repository;
mod filestore;
use crate::config::*;
use crate::repository::Repository;
use crate::package::PackageName;
use crate::package::PackageVersion;
use crate::util::executor::DummyExecutor;
use crate::package::Tree;
use crate::filestore::ReleaseStore;
use crate::filestore::StagingStore;
use crate::util::progress::ProgressBars;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::try_init()?;
    debug!("Debugging enabled");

    let cli = cli::cli();
    let cli = cli.get_matches();

    let mut config = ::config::Config::default();
    config
        .merge(::config::File::with_name("config"))?
        .merge(::config::Environment::with_prefix("YABOS"))?;
        // Add in settings from the environment (with a prefix of YABOS)
        // Eg.. `YABOS_DEBUG=1 ./target/app` would set the `debug` key
    //

    let config: Configuration = config.try_into::<NotValidatedConfiguration>()?.validate()?;
    let repo_path    = PathBuf::from(config.repository());
    let max_packages = count_pkg_files(&repo_path, ProgressBar::new_spinner());
    let progressbars = ProgressBars::setup(max_packages);

    let release_dir  = async {
        let variables = BTreeMap::new();
        let p = config.releases_directory(&variables)?;
        debug!("Loading release directory: {}", p.display());
        let r = ReleaseStore::load(&p, progressbars.release_loading.clone());
        if r.is_ok() {
            progressbars.release_loading.finish_with_message("Loaded releases successfully");
        } else {
            progressbars.release_loading.finish_with_message("Failed to load releases");
        }
        r
    };

    let staging_dir = async {
        let variables = BTreeMap::new();
        let p = config.staging_directory(&variables)?;
        debug!("Loading staging directory: {}", p.display());
        let r = StagingStore::load(&p, progressbars.staging_loading.clone());
        if r.is_ok() {
            progressbars.release_loading.finish_with_message("Loaded staging successfully");
        } else {
            progressbars.release_loading.finish_with_message("Failed to load staging");
        }
        r
    };

    let repo         = Repository::load(&repo_path, &progressbars.repo_loading)?;
    progressbars.repo_loading.finish_with_message("Repository loading finished");

    let pname = cli.value_of("package_name").map(String::from).map(PackageName::from).unwrap(); // safe by clap
    let pvers = cli.value_of("package_version").map(String::from).map(PackageVersion::from);

    let packages = if let Some(pvers) = pvers {
        repo.find(&pname, &pvers)
    } else {
        repo.find_by_name(&pname)
    };
    debug!("Found {} relevant packages", packages.len());

    let trees = tokio::stream::iter(packages.into_iter().cloned())
        .map(|p| {
            let mut tree = Tree::new();
            tree.add_package(p, &repo, &DummyExecutor::new(), progressbars.tree_building.clone())?;
            Ok(tree)
        })
        .collect::<Result<Vec<_>>>()
        .await?;

    debug!("Trees loaded: {:?}", trees);
    let mut out = std::io::stderr();
    for tree in trees {
        tree.debug_print(&mut out)?;
    }

    progressbars.root.join().map_err(Error::from)
}

fn count_pkg_files(p: &Path, progress: ProgressBar) -> u64 {
    WalkDir::new(p)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|d| d.file_type().is_file())
        .filter(|f| f.path().file_name().map(|name| name == "pkg.toml").unwrap_or(false))
        .inspect(|_| progress.tick())
        .count() as u64
}

