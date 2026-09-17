//! Turning a registry manifest into installed launch data.
//!
//! Selection is explicit: the host platform is resolved from a table and an
//! agent that publishes nothing for this host is reported, never approximated.
//! Installation writes only under the Batey-managed install root, checks
//! registry integrity metadata when the manifest supplies it, and extracts
//! through the strict archive rules.
use super::archive::{self, ArchiveKind};
use super::client::{HttpFetch, ProgressReporter, MAX_DOWNLOAD_BYTES};
use super::manifest::{DistributionKind, RegistryAgent};
use super::platform::PlatformTarget;
use crate::agents::installed::InstalledDistribution;
use crate::agents::operations::AgentOperationStage;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which distribution an install will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallPlan {
    pub kind: DistributionKind,
    /// Present only for a binary distribution.
    pub target: Option<PlatformTarget>,
}

/// Chooses the distribution to install.
///
/// With no preference, a native binary for this host wins, because it needs
/// no extra runtime. `npx` and `uvx` follow in that order.
pub fn select(
    agent: &RegistryAgent,
    preferred: Option<DistributionKind>,
    host: Option<PlatformTarget>,
) -> anyhow::Result<InstallPlan> {
    let distribution = &agent.distribution;
    match preferred {
        Some(DistributionKind::Binary) => {
            let host = host.ok_or_else(|| unsupported_host(agent))?;
            anyhow::ensure!(
                distribution.binary.contains_key(&host),
                "{} publishes no binary for {host}. It publishes: {}",
                agent.id,
                describe_binary_targets(agent)
            );
            Ok(InstallPlan {
                kind: DistributionKind::Binary,
                target: Some(host),
            })
        }
        Some(kind @ DistributionKind::Npx) => {
            anyhow::ensure!(
                distribution.npx.is_some(),
                "{} publishes no npx distribution",
                agent.id
            );
            Ok(InstallPlan { kind, target: None })
        }
        Some(kind @ DistributionKind::Uvx) => {
            anyhow::ensure!(
                distribution.uvx.is_some(),
                "{} publishes no uvx distribution",
                agent.id
            );
            Ok(InstallPlan { kind, target: None })
        }
        None => {
            if let Some(host) = host {
                if distribution.binary.contains_key(&host) {
                    return Ok(InstallPlan {
                        kind: DistributionKind::Binary,
                        target: Some(host),
                    });
                }
            }
            if distribution.npx.is_some() {
                return Ok(InstallPlan {
                    kind: DistributionKind::Npx,
                    target: None,
                });
            }
            if distribution.uvx.is_some() {
                return Ok(InstallPlan {
                    kind: DistributionKind::Uvx,
                    target: None,
                });
            }
            Err(unsupported_distribution(agent, host))
        }
    }
}

fn unsupported_host(agent: &RegistryAgent) -> anyhow::Error {
    anyhow::anyhow!(
        "{} needs a platform binary, but {} is not a registry platform",
        agent.id,
        PlatformTarget::host_description()
    )
}

fn unsupported_distribution(agent: &RegistryAgent, host: Option<PlatformTarget>) -> anyhow::Error {
    let host = host.map_or_else(PlatformTarget::host_description, |target| {
        target.as_str().to_string()
    });
    anyhow::anyhow!(
        "{} has no distribution this host can install. Host: {host}. Published: {}",
        agent.id,
        describe_distributions(agent)
    )
}

fn describe_distributions(agent: &RegistryAgent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !agent.distribution.binary.is_empty() {
        parts.push(format!("binary ({})", describe_binary_targets(agent)));
    }
    if agent.distribution.npx.is_some() {
        parts.push("npx".into());
    }
    if agent.distribution.uvx.is_some() {
        parts.push("uvx".into());
    }
    if parts.is_empty() {
        return "nothing".into();
    }
    parts.join(", ")
}

fn describe_binary_targets(agent: &RegistryAgent) -> String {
    agent
        .distribution
        .binary
        .keys()
        .map(|target| target.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Everything an installed record needs after the files are in place.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedInstall {
    pub distribution: InstalledDistribution,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub install_dir: Option<PathBuf>,
}

/// Downloads and installs the selected distribution.
///
/// `install_root` is the Batey-managed agent directory. Binary files land in
/// `<install_root>/<agent_id>/<version>`, a deterministic location, after
/// extraction into a staging directory beside it. `npx` and `uvx` write
/// nothing: they only record the pinned package spec.
pub async fn prepare(
    agent: &RegistryAgent,
    plan: InstallPlan,
    http: &dyn HttpFetch,
    install_root: &Path,
    agent_id: &str,
) -> anyhow::Result<PreparedInstall> {
    prepare_with_progress(agent, plan, http, install_root, agent_id, None, None).await
}

/// Downloads and installs the selected distribution, emitting progress and
/// lifecycle stage transitions.
pub async fn prepare_with_progress(
    agent: &RegistryAgent,
    plan: InstallPlan,
    http: &dyn HttpFetch,
    install_root: &Path,
    agent_id: &str,
    on_stage: Option<Arc<dyn Fn(AgentOperationStage) + Send + Sync>>,
    on_progress: Option<ProgressReporter>,
) -> anyhow::Result<PreparedInstall> {
    match plan.kind {
        DistributionKind::Binary => {
            let target = plan
                .target
                .ok_or_else(|| anyhow::anyhow!("A binary install needs a platform target"))?;
            let spec = agent
                .distribution
                .binary
                .get(&target)
                .ok_or_else(|| anyhow::anyhow!("{} publishes no binary for {target}", agent.id))?
                .clone();
            if let Some(ref stage_cb) = on_stage {
                stage_cb(AgentOperationStage::Downloading);
            }
            let body = http
                .fetch_with_progress(spec.archive.clone(), MAX_DOWNLOAD_BYTES, on_progress)
                .await?;
            if let Some(ref stage_cb) = on_stage {
                stage_cb(AgentOperationStage::Verifying);
            }
            let integrity_verified = match &spec.sha256 {
                Some(expected) => {
                    archive::verify_sha256(&body, expected)?;
                    true
                }
                None => {
                    tracing::warn!(
                        agent = %agent.id,
                        %target,
                        "The registry published no sha256 for this archive"
                    );
                    false
                }
            };
            if let Some(ref stage_cb) = on_stage {
                stage_cb(AgentOperationStage::Extracting);
            }
            let kind = archive::detect(&spec.archive, &body)?;
            let raw_name = archive::file_name_from_url(&spec.archive);
            let install_dir = install_directory(install_root, agent_id, &agent.version)?;
            let command = extract_into_place(kind, &body, &install_dir, &raw_name, &spec.cmd)?;
            Ok(PreparedInstall {
                distribution: InstalledDistribution::Binary {
                    target,
                    archive: spec.archive,
                    sha256: spec.sha256,
                    cmd: spec.cmd,
                    args: spec.args.clone(),
                    env: spec.env.clone(),
                    integrity_verified,
                },
                command: command.display().to_string(),
                args: spec.args,
                env: spec.env,
                install_dir: Some(install_dir),
            })
        }
        DistributionKind::Npx => {
            if let Some(ref stage_cb) = on_stage {
                stage_cb(AgentOperationStage::Preparing);
            }
            let spec = agent
                .distribution
                .npx
                .clone()
                .ok_or_else(|| anyhow::anyhow!("{} publishes no npx distribution", agent.id))?;
            ensure_pinned(&spec.package, DistributionKind::Npx)?;
            let mut args = vec!["--yes".to_string(), spec.package.clone()];
            args.extend(spec.args.clone());
            Ok(PreparedInstall {
                distribution: InstalledDistribution::Npx {
                    package: spec.package,
                    args: spec.args,
                    env: spec.env.clone(),
                },
                command: "npx".into(),
                args,
                env: spec.env,
                install_dir: None,
            })
        }
        DistributionKind::Uvx => {
            if let Some(ref stage_cb) = on_stage {
                stage_cb(AgentOperationStage::Preparing);
            }
            let spec = agent
                .distribution
                .uvx
                .clone()
                .ok_or_else(|| anyhow::anyhow!("{} publishes no uvx distribution", agent.id))?;
            ensure_pinned(&spec.package, DistributionKind::Uvx)?;
            let mut args = vec![spec.package.clone()];
            args.extend(spec.args.clone());
            Ok(PreparedInstall {
                distribution: InstalledDistribution::Uvx {
                    package: spec.package,
                    args: spec.args,
                    env: spec.env.clone(),
                },
                command: "uvx".into(),
                args,
                env: spec.env,
                install_dir: None,
            })
        }
    }
}

/// The deterministic location one registry version installs into.
pub fn install_directory(
    install_root: &Path,
    agent_id: &str,
    version: &str,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        safe_component(agent_id) && safe_component(version),
        "Agent id or registry version is not a safe install path component"
    );
    Ok(install_root.join(agent_id).join(version))
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && Path::new(value).components().count() == 1
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

/// Extracts into a staging directory and moves it into place only when the
/// launch command is present, so a failed install never replaces a working
/// one with a broken tree.
fn extract_into_place(
    kind: ArchiveKind,
    body: &[u8],
    install_dir: &Path,
    raw_name: &str,
    cmd: &str,
) -> anyhow::Result<PathBuf> {
    let parent = install_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Install directory has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".staging-{}-{}",
        install_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&staging)?;
        archive::extract(kind, body, &staging, raw_name)?;
        let relative = launch_relative_path(cmd)?;
        let staged_command = staging.join(&relative);
        anyhow::ensure!(
            staged_command.is_file(),
            "The archive does not contain the launch command '{cmd}'"
        );
        archive::set_executable(&staged_command)?;
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir)?;
        }
        std::fs::rename(&staging, install_dir)?;
        Ok(install_dir.join(relative))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// The `cmd` of a binary target, as a path relative to the install directory.
pub fn launch_relative_path(cmd: &str) -> anyhow::Result<PathBuf> {
    let trimmed = cmd.trim();
    anyhow::ensure!(!trimmed.is_empty(), "The launch command is empty");
    archive::safe_relative_path(Path::new(trimmed))
}

/// Version specifiers that resolve again on every run. They are refused,
/// because an installed agent must launch from pinned data.
const FLOATING_VERSIONS: &[&str] = &["latest", "next", "*", "", "beta", "alpha", "canary"];

/// Splits a package spec into its name and its pinned version.
pub fn split_package(spec: &str, kind: DistributionKind) -> Option<(String, String)> {
    let spec = spec.trim();
    if kind == DistributionKind::Uvx {
        // Only the exact-match operators. A range such as `>=1.0` resolves
        // again on every run, so it never counts as a pinned version.
        for separator in ["===", "=="] {
            if let Some((name, version)) = spec.split_once(separator) {
                return Some((name.to_string(), version.to_string()));
            }
        }
    }
    // An npm scope starts with `@`, so the version separator is the next `@`.
    let (offset, rest) = match spec.strip_prefix('@') {
        Some(rest) => (1, rest),
        None => (0, spec),
    };
    let index = rest.find('@')? + offset + 1;
    Some((spec[..index - 1].to_string(), spec[index..].to_string()))
}

/// A package spec must name one exact version.
pub fn ensure_pinned(spec: &str, kind: DistributionKind) -> anyhow::Result<()> {
    let (name, version) = split_package(spec, kind).ok_or_else(|| {
        anyhow::anyhow!(
            "The {kind} package '{spec}' does not pin a version. Batey installs \
             pinned packages only, so a session never resolves a new version at startup."
        )
    })?;
    anyhow::ensure!(!name.trim().is_empty(), "The {kind} package name is empty");
    let version = version.trim();
    anyhow::ensure!(
        !FLOATING_VERSIONS.contains(&version.to_lowercase().as_str()),
        "The {kind} package '{spec}' pins the floating version '{version}'. \
         Batey installs an exact version only."
    );
    anyhow::ensure!(
        exact_version(version),
        "The {kind} package '{spec}' does not pin an exact version."
    );
    Ok(())
}

fn exact_version(version: &str) -> bool {
    let permitted = |part: &str| {
        !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let (release, build) = version
        .split_once('+')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let (numeric, pre) = release
        .split_once('-')
        .map_or((release, None), |(a, b)| (a, Some(b)));
    numeric.split('.').count() >= 2
        && numeric
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && pre.is_none_or(|part| part.split('.').all(permitted))
        && build.is_none_or(|part| part.split('.').all(permitted))
}

#[cfg(test)]
mod tests {
    use super::super::client::testing::FixtureFetch;
    use super::super::manifest::parse_catalog;
    use super::*;
    use std::io::Write;

    fn catalog(json: &str) -> super::super::manifest::RegistryCatalog {
        parse_catalog(json).unwrap()
    }

    fn both_kinds() -> super::super::manifest::RegistryCatalog {
        catalog(
            r#"{"version":"1.0.0","agents":[{
              "id":"example","name":"Example","version":"1.2.3","description":"d",
              "distribution":{
                "binary":{
                  "linux-x86_64":{"archive":"https://e.invalid/linux.tar.gz","cmd":"./example"},
                  "darwin-aarch64":{"archive":"https://e.invalid/darwin.tar.gz","cmd":"./example"}
                },
                "npx":{"package":"example@1.2.3"},
                "uvx":{"package":"example==1.2.3"}
              }}]}"#,
        )
    }

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, body, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, path, *body).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_native_binary_for_this_host_is_preferred() {
        let catalog = both_kinds();
        let agent = catalog.agent("example").unwrap();
        let plan = select(agent, None, Some(PlatformTarget::LinuxX86_64)).unwrap();
        assert_eq!(plan.kind, DistributionKind::Binary);
        assert_eq!(plan.target, Some(PlatformTarget::LinuxX86_64));
    }

    #[test]
    fn a_host_without_a_binary_falls_back_to_a_package_runtime() {
        let catalog = both_kinds();
        let agent = catalog.agent("example").unwrap();
        let plan = select(agent, None, Some(PlatformTarget::WindowsAarch64)).unwrap();
        assert_eq!(plan.kind, DistributionKind::Npx);
        assert_eq!(plan.target, None);
    }

    #[test]
    fn uvx_is_used_when_it_is_the_only_distribution() {
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"p","name":"P","version":"1.0.0",
                "description":"d","distribution":{"uvx":{"package":"p==1.0.0"}}}]}"#,
        );
        let plan = select(
            catalog.agent("p").unwrap(),
            None,
            Some(PlatformTarget::LinuxX86_64),
        )
        .unwrap();
        assert_eq!(plan.kind, DistributionKind::Uvx);
    }

    #[test]
    fn an_unsupported_distribution_is_reported_clearly() {
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"b","name":"B","version":"1.0.0",
                "description":"d","distribution":{"binary":{
                  "darwin-aarch64":{"archive":"https://e.invalid/a.tar.gz","cmd":"./b"}}}}]}"#,
        );
        let agent = catalog.agent("b").unwrap();
        let error = select(agent, None, Some(PlatformTarget::LinuxX86_64)).unwrap_err();
        assert!(error.to_string().contains("linux-x86_64"), "{error}");
        assert!(error.to_string().contains("darwin-aarch64"), "{error}");

        // An unsupported host is named too, not silently mapped onto a target.
        let error = select(agent, None, None).unwrap_err();
        assert!(error.to_string().contains("no distribution"), "{error}");
    }

    #[test]
    fn an_explicit_preference_is_honoured_or_refused() {
        let catalog = both_kinds();
        let agent = catalog.agent("example").unwrap();
        assert_eq!(
            select(
                agent,
                Some(DistributionKind::Uvx),
                Some(PlatformTarget::LinuxX86_64)
            )
            .unwrap()
            .kind,
            DistributionKind::Uvx
        );
        let error = select(
            agent,
            Some(DistributionKind::Binary),
            Some(PlatformTarget::WindowsX86_64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("windows-x86_64"), "{error}");

        let npx_only = catalog_npx_only();
        let error = select(
            npx_only.agent("n").unwrap(),
            Some(DistributionKind::Binary),
            Some(PlatformTarget::LinuxX86_64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no binary"), "{error}");
        let error = select(
            npx_only.agent("n").unwrap(),
            Some(DistributionKind::Uvx),
            Some(PlatformTarget::LinuxX86_64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no uvx"), "{error}");
    }

    fn catalog_npx_only() -> super::super::manifest::RegistryCatalog {
        catalog(
            r#"{"version":"1.0.0","agents":[{"id":"n","name":"N","version":"1.0.0",
                "description":"d","distribution":{"npx":{"package":"n@1.0.0"}}}]}"#,
        )
    }

    #[tokio::test]
    async fn a_binary_install_verifies_extracts_and_lands_in_a_deterministic_place() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_bytes = tar_gz(&[("example", b"#!/bin/sh\nexit 0\n", 0o755)]);
        let digest = archive::sha256_hex(&archive_bytes);
        let catalog = catalog(&format!(
            r#"{{"version":"1.0.0","agents":[{{"id":"example","name":"E","version":"1.2.3",
                "description":"d","distribution":{{"binary":{{"linux-x86_64":{{
                  "archive":"https://e.invalid/example.tar.gz","sha256":"{digest}",
                  "cmd":"./example","args":["serve"],"env":{{"E":"1"}}}}}}}}}}]}}"#
        ));
        let agent = catalog.agent("example").unwrap();
        let http = FixtureFetch::new().with("https://e.invalid/example.tar.gz", archive_bytes);
        let plan = InstallPlan {
            kind: DistributionKind::Binary,
            target: Some(PlatformTarget::LinuxX86_64),
        };

        let prepared = prepare(agent, plan, &http, tmp.path(), "example")
            .await
            .unwrap();
        let expected_dir = tmp.path().join("example").join("1.2.3");
        assert_eq!(
            prepared.install_dir.as_deref(),
            Some(expected_dir.as_path())
        );
        assert_eq!(
            prepared.command,
            expected_dir.join("example").display().to_string()
        );
        assert_eq!(prepared.args, vec!["serve"]);
        assert_eq!(prepared.env["E"], "1");
        assert!(Path::new(&prepared.command).is_file());
        match prepared.distribution {
            InstalledDistribution::Binary {
                integrity_verified,
                target,
                ..
            } => {
                assert!(integrity_verified);
                assert_eq!(target, PlatformTarget::LinuxX86_64);
            }
            other => panic!("unexpected distribution {other:?}"),
        }
        // Nothing is left staged.
        let staged: Vec<_> = std::fs::read_dir(tmp.path().join("example"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging"))
            .collect();
        assert!(staged.is_empty());
    }

    #[tokio::test]
    async fn a_failed_integrity_check_installs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_bytes = tar_gz(&[("example", b"payload", 0o755)]);
        let catalog = catalog(&format!(
            r#"{{"version":"1.0.0","agents":[{{"id":"example","name":"E","version":"1.2.3",
                "description":"d","distribution":{{"binary":{{"linux-x86_64":{{
                  "archive":"https://e.invalid/example.tar.gz","sha256":"{}",
                  "cmd":"./example"}}}}}}}}]}}"#,
            "0".repeat(64)
        ));
        let http = FixtureFetch::new().with("https://e.invalid/example.tar.gz", archive_bytes);
        let plan = InstallPlan {
            kind: DistributionKind::Binary,
            target: Some(PlatformTarget::LinuxX86_64),
        };
        let error = prepare(
            catalog.agent("example").unwrap(),
            plan,
            &http,
            tmp.path(),
            "example",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("integrity"), "{error}");
        assert!(!tmp.path().join("example").join("1.2.3").exists());
    }

    /// A failed extraction must not leave a staging directory or replace a
    /// working install.
    #[tokio::test]
    async fn a_failed_extraction_leaves_the_previous_install_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tar_gz(&[("example", b"good", 0o755)]);
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"example","name":"E","version":"1.2.3",
                "description":"d","distribution":{"binary":{"linux-x86_64":{
                  "archive":"https://e.invalid/example.tar.gz","cmd":"./example"}}}}]}"#,
        );
        let plan = InstallPlan {
            kind: DistributionKind::Binary,
            target: Some(PlatformTarget::LinuxX86_64),
        };
        let http = FixtureFetch::new().with("https://e.invalid/example.tar.gz", good);
        prepare(
            catalog.agent("example").unwrap(),
            plan,
            &http,
            tmp.path(),
            "example",
        )
        .await
        .unwrap();
        let installed = tmp.path().join("example/1.2.3/example");
        assert_eq!(std::fs::read(&installed).unwrap(), b"good");

        // The same version now serves an archive without the launch command.
        let wrong = tar_gz(&[("other", b"bad", 0o755)]);
        let http = FixtureFetch::new().with("https://e.invalid/example.tar.gz", wrong);
        let error = prepare(
            catalog.agent("example").unwrap(),
            plan,
            &http,
            tmp.path(),
            "example",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("launch command"), "{error}");
        assert_eq!(std::fs::read(&installed).unwrap(), b"good");
        let staged: Vec<_> = std::fs::read_dir(tmp.path().join("example"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging"))
            .collect();
        assert!(staged.is_empty(), "a staging directory was left behind");
    }

    /// `tar::Builder` refuses a traversing path, so the hostile archive is
    /// written through the header directly.
    fn tar_gz_escaping(path: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        let name = &mut header.as_gnu_mut().expect("gnu header").name;
        name.fill(0);
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn an_archive_that_escapes_the_install_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let evil = tar_gz_escaping("../escape");
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"example","name":"E","version":"1.2.3",
                "description":"d","distribution":{"binary":{"linux-x86_64":{
                  "archive":"https://e.invalid/example.tar.gz","cmd":"./example"}}}}]}"#,
        );
        let http = FixtureFetch::new().with("https://e.invalid/example.tar.gz", evil);
        let plan = InstallPlan {
            kind: DistributionKind::Binary,
            target: Some(PlatformTarget::LinuxX86_64),
        };
        assert!(prepare(
            catalog.agent("example").unwrap(),
            plan,
            &http,
            tmp.path(),
            "example"
        )
        .await
        .is_err());
        assert!(!tmp.path().join("escape").exists());
    }

    /// A `cmd` that points outside the install directory is refused before
    /// anything is moved into place.
    #[test]
    fn a_launch_command_cannot_escape_the_install_directory() {
        assert_eq!(
            launch_relative_path("./example").unwrap(),
            PathBuf::from("example")
        );
        assert_eq!(
            launch_relative_path("bin/agent.exe").unwrap(),
            PathBuf::from("bin/agent.exe")
        );
        assert!(launch_relative_path("../../bin/sh").is_err());
        assert!(launch_relative_path("/bin/sh").is_err());
        assert!(launch_relative_path("  ").is_err());
    }

    #[tokio::test]
    async fn package_installs_snapshot_the_exact_spec_and_write_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = both_kinds();
        let agent = catalog.agent("example").unwrap();
        let http = FixtureFetch::new();

        let npx = prepare(
            agent,
            InstallPlan {
                kind: DistributionKind::Npx,
                target: None,
            },
            &http,
            tmp.path(),
            "example",
        )
        .await
        .unwrap();
        assert_eq!(npx.command, "npx");
        assert_eq!(npx.args, vec!["--yes", "example@1.2.3"]);
        assert_eq!(npx.install_dir, None);
        assert_eq!(
            npx.distribution,
            InstalledDistribution::Npx {
                package: "example@1.2.3".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            }
        );

        let uvx = prepare(
            agent,
            InstallPlan {
                kind: DistributionKind::Uvx,
                target: None,
            },
            &http,
            tmp.path(),
            "example",
        )
        .await
        .unwrap();
        assert_eq!(uvx.command, "uvx");
        assert_eq!(uvx.args, vec!["example==1.2.3"]);

        // No network call and no file were needed.
        assert_eq!(http.call_count(), 0);
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn package_specs_must_pin_an_exact_version() {
        assert_eq!(
            split_package("@scope/pkg@1.2.3", DistributionKind::Npx),
            Some(("@scope/pkg".into(), "1.2.3".into()))
        );
        assert_eq!(
            split_package("pkg@1.2.3", DistributionKind::Npx),
            Some(("pkg".into(), "1.2.3".into()))
        );
        assert_eq!(
            split_package("pkg==1.2.3", DistributionKind::Uvx),
            Some(("pkg".into(), "1.2.3".into()))
        );
        assert_eq!(
            split_package("minion-code@0.1.44", DistributionKind::Uvx),
            Some(("minion-code".into(), "0.1.44".into()))
        );
        assert_eq!(split_package("@scope/pkg", DistributionKind::Npx), None);

        assert!(ensure_pinned("@scope/pkg@1.2.3", DistributionKind::Npx).is_ok());
        assert!(ensure_pinned("pkg==1.2.3", DistributionKind::Uvx).is_ok());
        for spec in [
            "@scope/pkg",
            "pkg",
            "pkg@latest",
            "pkg@LATEST",
            "pkg@*",
            "pkg@^1.2.3",
            "pkg@next",
            "pkg@",
            "pkg@1.x",
            "pkg@1.2.*",
            "pkg@1.2.3 || 2.0.0",
        ] {
            assert!(
                ensure_pinned(spec, DistributionKind::Npx).is_err(),
                "{spec} was accepted"
            );
        }
        assert!(ensure_pinned("pkg>=1.0", DistributionKind::Uvx).is_err());
        assert!(ensure_pinned("pkg==1.2.*", DistributionKind::Uvx).is_err());
    }

    #[test]
    fn install_directory_refuses_traversal_and_absolute_components() {
        let root = Path::new("/managed/agents");
        for version in ["../target", "../../target", "/tmp/target", "", "."] {
            assert!(
                install_directory(root, "agent", version).is_err(),
                "{version} was accepted"
            );
        }
        assert!(install_directory(root, "../agent", "1.2.3").is_err());
    }

    #[tokio::test]
    async fn an_unpinned_package_is_refused_before_anything_is_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"f","name":"F","version":"1.0.0",
                "description":"d","distribution":{"npx":{"package":"floating"}}}]}"#,
        );
        let http = FixtureFetch::new();
        let error = prepare(
            catalog.agent("f").unwrap(),
            InstallPlan {
                kind: DistributionKind::Npx,
                target: None,
            },
            &http,
            tmp.path(),
            "f",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("pin"), "{error}");
    }

    #[tokio::test]
    async fn a_raw_binary_download_is_installed_as_the_launch_command() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"sig","name":"Sig","version":"1.5.8",
                "description":"d","distribution":{"binary":{"linux-x86_64":{
                  "archive":"https://e.invalid/sigit-linux-amd64","cmd":"./sigit-linux-amd64"}}}}]}"#,
        );
        let http =
            FixtureFetch::new().with("https://e.invalid/sigit-linux-amd64", b"\x7fELF".to_vec());
        let prepared = prepare(
            catalog.agent("sig").unwrap(),
            InstallPlan {
                kind: DistributionKind::Binary,
                target: Some(PlatformTarget::LinuxX86_64),
            },
            &http,
            tmp.path(),
            "sig",
        )
        .await
        .unwrap();
        assert_eq!(
            prepared.command,
            tmp.path()
                .join("sig/1.5.8/sigit-linux-amd64")
                .display()
                .to_string()
        );
        assert!(Path::new(&prepared.command).is_file());
    }

    #[test]
    fn install_directories_are_deterministic() {
        let root = Path::new("/data/agents");
        assert_eq!(
            install_directory(root, "example", "1.2.3").unwrap(),
            PathBuf::from("/data/agents/example/1.2.3")
        );
    }

    #[tokio::test]
    async fn binary_install_reports_stages_and_byte_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_bytes = tar_gz(&[("prog", b"#!/bin/sh\nexit 0\n", 0o755)]);
        let digest = archive::sha256_hex(&archive_bytes);
        let json = format!(
            "{{\"version\":\"1.0.0\",\"agents\":[{{\"id\":\"prog\",\"name\":\"P\",\"version\":\"1.0.0\",\"description\":\"d\",\"distribution\":{{\"binary\":{{\"linux-x86_64\":{{\"archive\":\"https://e.invalid/prog.tar.gz\",\"sha256\":\"{}\",\"cmd\":\"./prog\"}}}}}}}}]}}",
            digest
        );
        let catalog = catalog(&json);
        let agent = catalog.agent("prog").unwrap();

        let stages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stages_rec = stages.clone();
        let stage_cb = Arc::new(move |stage| {
            stages_rec.lock().unwrap().push(stage);
        });

        let progresses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let progresses_rec = progresses.clone();
        let progress_cb = Arc::new(move |dl, total| {
            progresses_rec.lock().unwrap().push((dl, total));
        });

        let http = FixtureFetch::new().with("https://e.invalid/prog.tar.gz", archive_bytes);
        let plan = InstallPlan {
            kind: DistributionKind::Binary,
            target: Some(PlatformTarget::LinuxX86_64),
        };

        prepare_with_progress(
            agent,
            plan,
            &http,
            tmp.path(),
            "prog",
            Some(stage_cb),
            Some(progress_cb),
        )
        .await
        .unwrap();

        let recorded_stages = stages.lock().unwrap().clone();
        assert_eq!(
            recorded_stages,
            vec![
                AgentOperationStage::Downloading,
                AgentOperationStage::Verifying,
                AgentOperationStage::Extracting,
            ]
        );

        let recorded_progress = progresses.lock().unwrap().clone();
        assert!(!recorded_progress.is_empty());
    }

    #[tokio::test]
    async fn npx_install_reports_preparing_stage() {
        let catalog = catalog(
            r#"{"version":"1.0.0","agents":[{"id":"n","name":"N","version":"1.0.0",
                "description":"d","distribution":{"npx":{"package":"@test/pkg@1.0.0"}}}]}"#,
        );
        let agent = catalog.agent("n").unwrap();
        let stages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stages_rec = stages.clone();
        let stage_cb = Arc::new(move |stage| {
            stages_rec.lock().unwrap().push(stage);
        });

        let http = FixtureFetch::new();
        let plan = InstallPlan {
            kind: DistributionKind::Npx,
            target: None,
        };

        prepare_with_progress(
            agent,
            plan,
            &http,
            Path::new("/tmp"),
            "n",
            Some(stage_cb),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            stages.lock().unwrap().clone(),
            vec![AgentOperationStage::Preparing]
        );
    }
}
