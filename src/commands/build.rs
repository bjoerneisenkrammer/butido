use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use clap::ArgMatches;
use colored::Colorize;
use diesel::ExpressionMethods;
use diesel::PgConnection;
use diesel::QueryDsl;
use diesel::RunQueryDsl;
use itertools::Itertools;
use log::{debug, info, warn, trace};
use tokio::stream::StreamExt;
use tokio::sync::RwLock;

use crate::config::*;
use crate::filestore::ReleaseStore;
use crate::filestore::StagingStore;
use crate::filestore::path::StoreRoot;
use crate::job::JobResource;
use crate::job::JobSet;
use crate::log::LogItem;
use crate::orchestrator::OrchestratorSetup;
use crate::package::PackageName;
use crate::package::PackageVersion;
use crate::package::Shebang;
use crate::package::Tree;
use crate::repository::Repository;
use crate::schema;
use crate::source::SourceCache;
use crate::util::EnvironmentVariableName;
use crate::util::docker::ImageName;
use crate::util::progress::ProgressBars;

pub async fn build(repo_root: &Path,
               matches: &ArgMatches,
               progressbars: ProgressBars,
               database_connection: PgConnection,
               config: &Configuration,
               repo: Repository,
               repo_path: &Path,
               max_packages: u64)
    -> Result<()>
{
    use crate::db::models::{
        EnvVar,
        Package,
        Job,
        GitHash,
        Image,
        Submit,
    };

    let now = chrono::offset::Local::now().naive_local();
    let submit_id = uuid::Uuid::new_v4();
    println!("Submit {}, started {}", submit_id, now);

    let shebang = Shebang::from({
        matches.value_of("shebang")
            .map(String::from)
            .unwrap_or_else(|| config.shebang().clone())
    });

    let image_name = matches.value_of("image").map(String::from).map(ImageName::from).unwrap(); // safe by clap
    if config.docker().verify_images_present() {
        if !config.docker().images().iter().any(|img| image_name == *img) {
            return Err(anyhow!("Requested build image {} is not in the configured images"))
                .with_context(|| anyhow!("Available images: {:?}", config.docker().images()))
                .with_context(|| anyhow!("Image present verification failed"))
                .map_err(Error::from)
        }
    }

    debug!("Getting repository HEAD");
    let hash_str   = crate::util::git::get_repo_head_commit_hash(repo_path)?;
    trace!("Repository HEAD = {}", hash_str);
    let phases = config.available_phases();

    let endpoint_configurations = config.docker().endpoints()
        .iter()
        .cloned()
        .map(|ep_cfg| {
            crate::endpoint::EndpointConfiguration::builder()
                .endpoint(ep_cfg)
                .required_images(config.docker().images().clone())
                .required_docker_versions(config.docker().docker_versions().clone())
                .required_docker_api_versions(config.docker().docker_api_versions().clone())
                .build()
        })
        .collect();
    info!("Endpoint config build");

    let pname = matches.value_of("package_name")
        .map(String::from)
        .map(PackageName::from)
        .unwrap(); // safe by clap

    let pvers = matches.value_of("package_version")
        .map(String::from)
        .map(PackageVersion::from);
    info!("We want {} ({:?})", pname, pvers);

    let additional_env = matches.values_of("env")
        .unwrap_or_default()
        .map(|s| {
            let v = s.split("=").collect::<Vec<_>>();
            Ok((
                 EnvironmentVariableName::from(*v.get(0).ok_or_else(|| anyhow!("Environment variable has no key: {}", s))?),
                 String::from(*v.get(1).ok_or_else(|| anyhow!("Environment variable has no key: {}", s))?)
            ))
        })
        .collect::<Result<Vec<(EnvironmentVariableName, String)>>>()?;

    let packages = if let Some(pvers) = pvers {
        repo.find(&pname, &pvers)
    } else {
        repo.find_by_name(&pname)
    };
    debug!("Found {} relevant packages", packages.len());

    // We only support building one package per call.
    // Everything else is invalid
    if packages.len() > 1 {
        return Err(anyhow!("Found multiple packages ({}). Cannot decide which one to build", packages.len()))
    }
    let package = *packages.get(0).ok_or_else(|| anyhow!("Found no package."))?;

    let release_dir  = {
        let bar_release_loading = progressbars.bar();
        bar_release_loading.set_length(max_packages);

        let p = config.releases_directory();
        debug!("Loading release directory: {}", p.display());
        let r = ReleaseStore::load(StoreRoot::new(p.clone())?, bar_release_loading.clone());
        if r.is_ok() {
            bar_release_loading.finish_with_message("Loaded releases successfully");
        } else {
            bar_release_loading.finish_with_message("Failed to load releases");
        }
        r.map(RwLock::new).map(Arc::new)?
    };

    let (staging_store, staging_dir) = {
        let bar_staging_loading = progressbars.bar();
        bar_staging_loading.set_length(max_packages);

        let p = if let Some(staging_dir) = matches.value_of("staging_dir").map(PathBuf::from) {
            info!("Setting staging dir to {} for this run", staging_dir.display());
            staging_dir
        } else {
            config.staging_directory().join(submit_id.hyphenated().to_string())
        };

        if !p.is_dir() {
            let _ = tokio::fs::create_dir_all(&p).await?;
        }

        debug!("Loading staging directory: {}", p.display());
        let r = StagingStore::load(StoreRoot::new(p.clone())?, bar_staging_loading.clone());
        if r.is_ok() {
            bar_staging_loading.finish_with_message("Loaded staging successfully");
        } else {
            bar_staging_loading.finish_with_message("Failed to load staging");
        }
        r.map(RwLock::new)
            .map(Arc::new)
            .map(|store| (store, p))?
    };

    let tree = {
        let bar_tree_building = progressbars.bar();
        bar_tree_building.set_length(max_packages);

        let mut tree = Tree::new();
        tree.add_package(package.clone(), &repo, bar_tree_building.clone())?;

        bar_tree_building.finish_with_message("Finished loading Tree");
        tree
    };

    let source_cache = SourceCache::new(config.source_cache_root().clone());

    if matches.is_present("no_verification") {
        warn!("No hash verification will be performed");
    } else {
        crate::commands::source::verify_impl(tree.all_packages().into_iter(), &source_cache, &progressbars)
            .await?;
    }

    // linting the package scripts
    if matches.is_present("no_lint") {
        warn!("No script linting will be performed!");
    } else {
        if let Some(linter) = crate::ui::find_linter_command(repo_root, config)? {
            let all_packages = tree.all_packages();
            let bar = progressbars.bar();
            bar.set_length(all_packages.len() as u64);
            bar.set_message("Linting package scripts...");

            let iter = all_packages.into_iter();
            let _ = crate::commands::util::lint_packages(iter, &linter, config, bar).await?;
        } else {
            warn!("No linter set in configuration, no script linting will be performed!");
        }
    } // linting

    tree.all_packages()
        .into_iter()
        .map(|pkg| {
            if let Some(allowlist) = pkg.allowed_images() {
                if !allowlist.contains(&image_name) {
                    return Err(anyhow!("Package {} {} is only allowed on: {}", pkg.name(), pkg.version(), allowlist.iter().join(", ")))
                }
            }

            if let Some(deniedlist) = pkg.denied_images() {
                if deniedlist.iter().any(|denied| image_name == *denied) {
                    return Err(anyhow!("Package {} {} is not allowed to be built on {}", pkg.name(), pkg.version(), image_name))
                }
            }

            Ok(())
        })
        .collect::<Result<Vec<()>>>()?;

    trace!("Setting up database jobs for Package, GitHash, Image");
    let db_package = async { Package::create_or_fetch(&database_connection, &package) };
    let db_githash = async { GitHash::create_or_fetch(&database_connection, &hash_str) };
    let db_image   = async { Image::create_or_fetch(&database_connection, &image_name) };
    let db_envs    = async {
        additional_env.clone()
            .into_iter()
            .map(|(k, v)| async {
                let k: EnvironmentVariableName = k; // hack to work around move semantics
                let v: String = v; // hack to work around move semantics
                EnvVar::create_or_fetch(&database_connection, &k, &v)
            })
            .collect::<futures::stream::FuturesUnordered<_>>()
            .collect::<Result<Vec<EnvVar>>>()
            .await
    };

    trace!("Running database jobs for Package, GitHash, Image");
    let (db_package, db_githash, db_image, db_envs) = tokio::join!(
        db_package,
        db_githash,
        db_image,
        db_envs
    );

    let (db_package, db_githash, db_image, _) = (db_package?, db_githash?, db_image?, db_envs?);

    trace!("Database jobs for Package, GitHash, Image finished successfully");
    trace!("Creating Submit in database");
    let submit = Submit::create(&database_connection,
        &tree,
        &now,
        &submit_id,
        &db_image,
        &db_package,
        &db_githash)?;
    trace!("Creating Submit in database finished successfully: {:?}", submit);

    trace!("Setting up job sets");
    let resources: Vec<JobResource> = additional_env.into_iter().map(JobResource::from).collect();
    let jobsets = JobSet::sets_from_tree(tree, shebang, image_name, phases.clone(), resources)?;
    trace!("Setting up job sets finished successfully");

    trace!("Setting up Orchestrator");
    let database_connection = Arc::new(database_connection);
    let orch = OrchestratorSetup::builder()
        .progress_generator(progressbars)
        .endpoint_config(endpoint_configurations)
        .staging_store(staging_store)
        .release_store(release_dir)
        .database(database_connection.clone())
        .source_cache(source_cache)
        .submit(submit)
        .log_dir(if matches.is_present("write-log-file") { Some(config.log_dir().clone()) } else { None })
        .jobsets(jobsets)
        .config(config)
        .build()
        .setup()
        .await?;

    info!("Running orchestrator...");
    let mut artifacts = vec![];
    let errors      = orch.run(&mut artifacts).await?;
    let out         = std::io::stdout();
    let mut outlock = out.lock();

    if !artifacts.is_empty() {
        writeln!(outlock, "Packages created:")?;
    }
    artifacts.into_iter()
        .map(|artifact| writeln!(outlock, "-> {}", staging_dir.join(artifact.path).display()).map_err(Error::from))
        .collect::<Result<_>>()?;

    let mut had_error = false;
    for (job_uuid, error) in errors {
        had_error = true;
        for cause in error.chain() {
            writeln!(outlock, "{}: {}", "[ERROR]".red(), cause)?;
        }

        let data = schema::jobs::table
            .filter(schema::jobs::dsl::uuid.eq(job_uuid))
            .inner_join(schema::packages::table)
            .first::<(Job, Package)>(database_connection.as_ref())?;

        let number_log_lines = *config.build_error_lines();
        writeln!(outlock, "Last {} lines of Job {}", number_log_lines, job_uuid)?;
        writeln!(outlock, "for package {} {}\n\n", data.1.name, data.1.version)?;

        let parsed_log = crate::log::ParsedLog::build_from(&data.0.log_text)?;
        let mut last_phase = None;
        let mut error_catched = false;
        let lines = parsed_log.iter()
            .map(|line_item| match line_item {
                LogItem::Line(s)         => Ok(String::from_utf8(s.to_vec())?.normal()),
                LogItem::Progress(u)     => Ok(format!("#BUTIDO:PROGRESS:{}", u).bright_black()),
                LogItem::CurrentPhase(p) => {
                    if !error_catched {
                        last_phase = Some(p.clone());
                    }
                    Ok(format!("#BUTIDO:PHASE:{}", p).bright_black())
                },
                LogItem::State(Ok(()))   => Ok(format!("#BUTIDO:STATE:OK").green()),
                LogItem::State(Err(s))   => {
                    error_catched = true;
                    Ok(format!("#BUTIDO:STATE:ERR:{}", s).red())
                },
            })
            .collect::<Result<Vec<_>>>()?;

        lines
            .iter()
            .enumerate()
            .skip({
                if lines.len() > number_log_lines {
                    lines.len() - number_log_lines
                } else {
                    lines.len()
                }
            })
            .map(|(i, line)| {
                writeln!(outlock, "{:>4} | {}", i, line).map_err(Error::from)
            })
            .collect::<Result<()>>()?;

        writeln!(outlock, "\n\n")?;
        if error_catched {
            if let Some(last_phase) = last_phase {
                writeln!(outlock, "\tJob errored in Phase '{}'", last_phase)?;
            }
            writeln!(outlock, "\n\n")?;
        } else {
            writeln!(outlock, "{}", "Error seems not to be caused by packaging script.".red())?;
        }
    }

    if had_error {
        Err(anyhow!("One or multiple errors during build"))
    } else {
        Ok(())
    }
}
