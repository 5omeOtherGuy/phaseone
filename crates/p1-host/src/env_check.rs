//! Catalog inspection against the installed host, never a provider access or tool dispatch.

use std::path::{Path, PathBuf};

use p1_assembly::{Catalog, assemble, load_mcp_servers, load_modules_lock};
use serde::Serialize;

use crate::HostDeps;
use crate::cli::Options;

#[derive(Serialize)]
struct Report {
    environment: String,
    failures: Vec<String>,
    file: PathBuf,
    ok: bool,
    profiles: Vec<String>,
}

pub(crate) fn check(deps: &mut HostDeps, options: &Options, roots: &[PathBuf], json: bool) -> i32 {
    // Explicit catalogs are checked against the shipped product, not an unrelated
    // user override. No explicit root means the exact merged view used by a run.
    let original = deps.environment_dirs.clone();
    if !roots.is_empty() {
        deps.environment_dirs = roots.iter().map(|root| root.join("environments")).collect();
        deps.environment_dirs
            .extend(crate::routes::shipped_environment_dirs());
    }
    // The normal catalog also eagerly snapshots GitHub tokens for its HTTP
    // tools. Inspection needs locations only, never credential variables.
    let original_env = deps.shell_env.take();
    let location_names = ["HOME", "XDG_CONFIG_HOME", "P1_CONFIG_DIR"];
    deps.shell_env = Some(match &original_env {
        Some(snapshot) => snapshot
            .iter()
            .filter(|(name, _)| {
                location_names
                    .iter()
                    .any(|allowed| name == std::ffi::OsStr::new(allowed))
            })
            .cloned()
            .collect(),
        None => location_names
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)))
            .collect(),
    });
    let reports = inspect(deps, options, roots);
    deps.shell_env = original_env;
    deps.environment_dirs = original;
    let failed = reports.iter().filter(|report| !report.ok).count();
    if json {
        let value = serde_json::to_value(&reports).expect("catalog reports serialize");
        crate::run::write_stdout(deps, &(p1_json_order::canonical_json(&value) + "\n"));
    } else {
        for report in &reports {
            let status = if report.ok {
                "ok".into()
            } else {
                report.failures.join("; ").replace(['\n', '\r'], " ")
            };
            crate::run::write_stdout(deps, &format!("{}  {status}\n", report.environment));
        }
        crate::run::write_stdout(
            deps,
            &format!(
                "{} environments: {} ok, {failed} failed\n",
                reports.len(),
                reports.len() - failed,
            ),
        );
    }
    if failed == 0 {
        crate::run::EXIT_OK
    } else {
        crate::run::EXIT_FAILURE
    }
}

fn inspect(deps: &HostDeps, options: &Options, roots: &[PathBuf]) -> Vec<Report> {
    let directories = &deps.environment_dirs;
    let mut global = Vec::new();
    for root in roots {
        if !root.join("environments").is_dir() {
            global.push(format!(
                "{}: missing environments directory",
                root.display()
            ));
        }
    }
    // Parse the whole layered lock even when no environment selects an entry.
    if let Err(error) = load_modules_lock(directories) {
        global.push(error.to_string());
    }
    let names = match crate::models::environment_names(directories) {
        Ok(names) => names,
        Err(message) => {
            global.push(message);
            Vec::new()
        }
    };
    let workspace = crate::run::resolve_workspace(options);
    let catalog = crate::run::inspection_catalog(deps, options);
    if let Err(message) = &catalog {
        global.push(message.clone());
    }
    if let Err(message) = &workspace {
        global.push(message.clone());
    }
    let mut reports = Vec::new();
    for name in names {
        let file = directories
            .iter()
            .map(|dir| dir.join(&name).join("environment.toml"))
            .find(|path| path.is_file())
            .expect("enumerated environment has a file");
        let mut report = Report {
            environment: name.clone(),
            failures: Vec::new(),
            file,
            ok: false,
            profiles: Vec::new(),
        };
        for message in &global {
            fail(&mut report, message);
        }
        if let Err(error) = load_mcp_servers(&name, directories) {
            fail(&mut report, &error.to_string());
        }
        // A broken route binding must not hide a separate default assembly error.
        if let (Ok(catalog), Ok(workspace)) = (&catalog, &workspace)
            && let Err(message) = inspect_environment(deps, catalog, &name, None, workspace)
        {
            fail(&mut report, &message);
        }
        match crate::models::environment_models(&name, directories) {
            Err(message) => fail(&mut report, &message),
            Ok(models) => {
                report.profiles = models.iter().map(|model| model.profile.clone()).collect();
                if let (Ok(catalog), Ok(workspace)) = (&catalog, &workspace) {
                    for model in models {
                        if let Err(message) = inspect_environment(
                            deps,
                            catalog,
                            &name,
                            Some(&model.profile),
                            workspace,
                        ) {
                            fail(
                                &mut report,
                                &format!("profile `{}`: {message}", model.profile),
                            );
                        }
                    }
                }
            }
        }
        report.ok = report.failures.is_empty();
        reports.push(report);
    }
    #[cfg(feature = "delegation")]
    if let (Ok(catalog), Ok(workspace)) = (&catalog, &workspace) {
        for (environment, message) in crate::catalog::subagents::inspect(
            directories,
            catalog,
            &crate::run::substitutions(deps, workspace),
        ) {
            if let Some(report) = reports
                .iter_mut()
                .find(|report| report.environment == environment)
            {
                fail(report, &message);
                report.ok = false;
            } else {
                reports.push(Report {
                    environment,
                    file: directories
                        .iter()
                        .flat_map(|directory| {
                            [
                                directory.join("../subagents.toml"),
                                directory.join("subagents.toml"),
                            ]
                        })
                        .find(|path| path.is_file())
                        .expect("subagent failure has a configuration file"),
                    failures: vec![message],
                    ok: false,
                    profiles: Vec::new(),
                });
            }
        }
    }
    #[cfg(not(feature = "delegation"))]
    for directory in directories {
        if let Some(path) = [
            directory.join("../subagents.toml"),
            directory.join("subagents.toml"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        {
            reports.push(Report {
                environment: "subagents".into(),
                file: path.clone(),
                failures: vec![format!(
                    "{}: subagents need the delegation feature",
                    path.display()
                )],
                ok: false,
                profiles: Vec::new(),
            });
            break;
        }
    }
    if reports.is_empty() && !global.is_empty() {
        reports.push(Report {
            environment: "catalog".into(),
            file: PathBuf::new(),
            failures: global,
            ok: false,
            profiles: Vec::new(),
        });
    }
    reports.sort_by(|left, right| left.environment.cmp(&right.environment));
    reports
}

fn fail(report: &mut Report, message: &str) {
    let message = format!("{}: {message}", report.file.display());
    if !report.failures.contains(&message) {
        report.failures.push(message);
    }
}

fn inspect_environment(
    deps: &HostDeps,
    catalog: &Catalog,
    name: &str,
    profile: Option<&str>,
    workspace: &Path,
) -> Result<(), String> {
    let mut environment =
        p1_assembly::load_environment_for_profile(name, &deps.environment_dirs, profile)
            .map_err(|e| e.to_string())?;
    if let Some(profile) = profile {
        crate::models::apply(
            &mut environment,
            &crate::models::Choice {
                environment: name.into(),
                profile: Some(profile.into()),
                effort: None,
                account: None,
                route: None,
            },
            &deps.environment_dirs,
        )?;
    }
    #[cfg(feature = "delegation")]
    crate::catalog::delegation::with_worker_tools_from_sources(
        &mut environment,
        crate::catalog::delegation::enabled_capabilities(deps)?,
        Some(&deps.verified_sources),
    )?;
    crate::catalog::resolve_environment(&mut environment, &deps.environment_dirs)?;
    if environment.profile.is_some() {
        let route = crate::routes::load_route_by_id(&deps.environment_dirs, &environment.provider)?;
        crate::routes::check_static_credential_origin(&route).map_err(|message| {
            format!(
                "{} / {}: {message}",
                route.source.display(),
                route.account_source.display(),
            )
        })?;
        // The route's credential method is one this p1 has, and its table is one the method
        // accepts: a credential module's method is resolved here, not when the file is read
        // (ADR-0212).
        crate::auth::locations(deps)
            .method(&route.credential.kind)
            .and_then(|method| route.credential.validate_with(method.as_ref()))
            .map_err(|message| format!("{}: {message}", route.account_source.display()))?;
    }
    assemble(
        catalog,
        &environment,
        workspace,
        &crate::run::substitutions(deps, workspace),
    )
    .map_err(|error| format!("prompt.md / assembly: {error}"))?;
    Ok(())
}
