//! Installing and removing packages with apt (Debian, Ubuntu).
//!
//! Before approval Kelpy reads which of the packages are installed and asks apt what the change would do, without
//! doing it (`apt-get -s`): the card lists the packages installed or removed with them. Once approved ([`apply`]) it
//! refreshes the package lists when they are old, asks apt again and stops if installing would now remove a package
//! the card did not show, makes the change, and checks each package. An installation that fails removes the
//! packages it added, those that were not installed before. Removing always asks: a package's data may go with it.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use crate::executor::{RUN_TIME, captured};
use crate::inspect::run;
use crate::{Command, Os, Output, Recorded, RecordedKind, ToolError, invalid};

/// How long refreshing the package lists may take.
const REFRESH_TIME: Duration = Duration::from_secs(10 * 60);
/// How long installing or removing may take: large packages download slowly on some servers.
const CHANGE_TIME: Duration = Duration::from_secs(30 * 60);
/// Package lists older than this are refreshed before an installation.
const LISTS_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const LISTS: &str = "/var/lib/apt/lists";

/// An installation or a removal of packages: as asked, then with what apt would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageChange {
    pub packages: Vec<String>,
    pub install: bool,
    pub planned: Option<PackagePlanned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePlanned {
    /// What apt would do: one package a line, `+name version` installed, `-name version` removed; or why it is not
    /// known yet.
    pub preview: String,
    /// Whether the package lists are refreshed first.
    pub refresh: bool,
    /// The command that makes the change.
    pub command: Command,
    pub floor: Option<String>,
}

impl PackageChange {
    pub fn summary(&self) -> String {
        format!(
            "{} {}",
            if self.install { "Install" } else { "Remove" },
            self.packages.join(", ")
        )
    }

    /// What the card says besides the preview.
    pub fn notes(&self, planned: &PackagePlanned) -> Vec<String> {
        let mut notes = Vec::new();
        if planned.refresh {
            notes.push(
                "The package lists are refreshed first (apt-get update): what is installed may then differ a \
                 little from the list above. If it would remove a package the list does not show, nothing is \
                 installed."
                    .to_owned(),
            );
        }
        notes.push(if self.install {
            "Each package is checked afterwards; if the installation fails, the packages it added are removed."
                .to_owned()
        } else {
            "Each package is checked afterwards. Kelpy cannot undo a removal: the packages can be installed again."
                .to_owned()
        });
        notes
    }
}

/// A package's name, as apt takes it. A name ending in - or + would tell apt to remove or install it whatever the
/// command, and an architecture would not match the check afterwards: both are refused.
pub fn name(value: &str) -> Result<String, ToolError> {
    let fits = (1..=100).contains(&value.len())
        && value.starts_with(|char: char| char.is_ascii_alphanumeric())
        && value.ends_with(|char: char| char.is_ascii_alphanumeric())
        && value.chars().all(|char| {
            char.is_ascii_lowercase() || char.is_ascii_digit() || "+-._".contains(char)
        });
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(format!(
            "`{value}` is not a package name: lower-case letters, digits and + - . _, starting and ending with a \
             letter or a digit, without an architecture"
        )))
    }
}

/// Checks that apt is here, reads the packages and asks apt what the change would do. `Ok(Err(text))` when the
/// packages are already as asked.
pub async fn plan(
    change: &PackageChange,
    os: Os,
    installed: impl Fn(&str) -> bool,
) -> Result<Result<PackagePlanned, String>, ToolError> {
    if os != Os::Linux || !installed("apt-get") || !installed("dpkg-query") {
        return Err(invalid(
            "Kelpy installs packages with apt (Debian, Ubuntu) only, for now: use run_command",
        ));
    }
    let versions = versions(&change.packages).await?;
    let present: Vec<String> = change
        .packages
        .iter()
        .filter_map(|name| {
            versions
                .get(name)
                .map(|version| format!("{name} {version}"))
        })
        .collect();
    if change.install && present.len() == change.packages.len() {
        return Ok(Err(format!(
            "Already installed: {}. Nothing to change.",
            present.join(", ")
        )));
    }
    if !change.install && present.is_empty() {
        return Ok(Err(format!(
            "Not installed: {}. Nothing to change.",
            change.packages.join(", ")
        )));
    }
    let refresh = change.install && lists_are_old();
    let verb = if change.install { "install" } else { "remove" };
    if change.install && !refresh {
        virtual_packages(&change.packages, &versions).await?;
    }
    let (code, said) = captured(&simulation(change), None, RUN_TIME).await?;
    let preview = if code == 0 {
        preview(&said)
    } else if refresh {
        "Not known until the package lists are refreshed.".to_owned()
    } else {
        let problems: Vec<&str> = said.lines().filter(|line| line.starts_with("E:")).collect();
        return Err(invalid(format!(
            "apt cannot {verb} them: {}",
            if problems.is_empty() {
                said.trim().to_owned()
            } else {
                problems.join(" ")
            }
        )));
    };
    let floor = if !change.install {
        Some(
            "It removes packages: the programs stop working and their data may go with them, so it always asks."
                .to_owned(),
        )
    } else if preview.lines().any(|line| line.starts_with('-')) {
        Some(
            "Installing them removes other packages (the lines with -): their programs stop working, so it always \
             asks."
                .to_owned(),
        )
    } else if code != 0 {
        Some(
            "What apt installs or removes is not known until the package lists are refreshed, so it always asks."
                .to_owned(),
        )
    } else {
        None
    };
    Ok(Ok(PackagePlanned {
        command: command(change, !removed(&preview).is_empty()),
        preview,
        refresh,
        floor,
    }))
}

/// Fails on a package apt has no candidate for and that is not installed: a virtual package, which another package
/// provides, or an unknown one.
async fn virtual_packages(
    packages: &[String],
    installed: &std::collections::HashMap<String, String>,
) -> Result<(), ToolError> {
    let missing: Vec<&str> = packages
        .iter()
        .filter(|name| !installed.contains_key(*name))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut args = vec!["policy"];
    args.extend(&missing);
    let (_, said) = captured(&run("apt-cache", &args), None, RUN_TIME).await?;
    // `name:` then `  Candidate: version`, for each package apt knows.
    let mut current = None;
    let mut candidates = std::collections::HashSet::new();
    for line in said.lines() {
        if !line.starts_with(' ') && line.ends_with(':') {
            current = Some(line.trim_end_matches(':').to_owned());
        } else if let (Some(name), Some(candidate)) =
            (&current, line.trim().strip_prefix("Candidate: "))
            && candidate != "(none)"
        {
            candidates.insert(name.clone());
        }
    }
    let without: Vec<&str> = missing
        .into_iter()
        .filter(|name| !candidates.contains(*name))
        .collect();
    if without.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "apt has no package to install for {}: it may be a virtual package, provided by another one (name \
             that one), or unknown",
            without.join(", ")
        )))
    }
}

/// Makes the change and checks each package; an installation that fails removes what it added.
pub async fn apply(
    change: &PackageChange,
    planned: &PackagePlanned,
    output: &Output,
) -> Result<Vec<Recorded>, ToolError> {
    if planned.refresh {
        output.set_phase("refreshing the package lists");
        let update = run("apt-get", &["update"]);
        output.line(&format!("$ {}", update.display));
        let (code, said) = captured(&update, None, REFRESH_TIME).await?;
        output.text(&said);
        if code != 0 {
            return Err(ToolError::Unverified(
                "the package lists could not be refreshed: nothing was installed".to_owned(),
            ));
        }
    }
    if change.install {
        unshown_removals(change, planned).await?;
    }
    let before = everything_installed().await?;
    output.set_phase(if change.install {
        "installing packages"
    } else {
        "removing packages"
    });
    output.line(&format!("$ {}", planned.command.display));
    let (code, said) = captured(&planned.command, None, CHANGE_TIME).await?;
    output.text(&said);
    let versions = versions(&change.packages).await?;
    let missing: Vec<&String> = change
        .packages
        .iter()
        .filter(|name| versions.contains_key(*name) != change.install)
        .collect();
    if code == 0 && missing.is_empty() {
        for name in &change.packages {
            output.line(&match versions.get(name) {
                Some(version) => format!("{name} {version} is installed."),
                None => format!("{name} is not installed."),
            });
        }
        return Ok(change
            .packages
            .iter()
            .map(|name| Recorded {
                kind: RecordedKind::Package,
                subject: name.clone(),
                state: if change.install {
                    "installed"
                } else {
                    "absent"
                }
                .to_owned(),
                content: None,
                secret: false,
            })
            .collect());
    }
    let problem = if missing.is_empty() {
        format!("apt-get ended with exit code {code}")
    } else {
        format!(
            "{} {}",
            missing
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if change.install {
                "not installed"
            } else {
                "still installed"
            }
        )
    };
    if !change.install {
        return Err(ToolError::Unverified(problem));
    }
    output.set_phase("removing the packages the installation added");
    let after = everything_installed().await?;
    let added: Vec<&str> = after.difference(&before).map(String::as_str).collect();
    let undone = if added.is_empty() {
        "it added no package".to_owned()
    } else {
        // Removed, not purged: configuration files left by an earlier removal stay as they were.
        let mut remove = vec!["remove", "-y"];
        remove.extend(&added);
        let remove = unattended(run("apt-get", &remove));
        output.line(&format!("$ {}", remove.display));
        let (code, said) = captured(&remove, None, CHANGE_TIME).await?;
        output.text(&said);
        if code == 0 {
            format!("the packages it added were removed ({})", added.join(", "))
        } else {
            format!(
                "the packages it added could not all be removed ({})",
                added.join(", ")
            )
        }
    };
    Err(ToolError::Unverified(format!(
        "the installation failed ({problem}): {undone}"
    )))
}

/// Fails when installing would now remove a package the approved preview did not show: after the refresh, or
/// because the server changed while the card waited. The command's `--no-remove` covers the case where the preview
/// removed nothing; this check covers the others, and says why before anything is done.
async fn unshown_removals(
    change: &PackageChange,
    planned: &PackagePlanned,
) -> Result<(), ToolError> {
    let (code, said) = captured(&simulation(change), None, RUN_TIME).await?;
    if code != 0 {
        // The installation fails the same way, and says why.
        return Ok(());
    }
    let approved = removed(&planned.preview);
    let now = preview(&said);
    let unshown: Vec<&str> = removed(&now)
        .into_iter()
        .filter(|name| !approved.contains(name))
        .collect();
    if unshown.is_empty() {
        Ok(())
    } else {
        Err(ToolError::Unverified(format!(
            "installing them would now also remove {}, which the approved change did not show: nothing was              installed, plan it again",
            unshown.join(", ")
        )))
    }
}

/// apt's simulation of the change (`apt-get -s`): what it would install and remove, without doing it.
fn simulation(change: &PackageChange) -> Command {
    let mut args = vec![
        "-s",
        "-y",
        if change.install { "install" } else { "remove" },
    ];
    args.extend(change.packages.iter().map(String::as_str));
    run("apt-get", &args)
}

/// The packages a [`preview`] shows removed.
fn removed(preview: &str) -> BTreeSet<&str> {
    preview
        .lines()
        .filter_map(|line| line.strip_prefix('-')?.split_whitespace().next())
        .collect()
}

/// The command that makes the change, without questions. An installation whose preview removes nothing gets
/// `--no-remove`: apt then stops rather than remove a package nobody approved.
fn command(change: &PackageChange, removes: bool) -> Command {
    let mut args = if change.install {
        let mut args = vec![
            "install",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confdef",
            "-o",
            "Dpkg::Options::=--force-confold",
        ];
        if !removes {
            args.push("--no-remove");
        }
        args
    } else {
        vec!["remove", "-y"]
    };
    args.extend(change.packages.iter().map(String::as_str));
    unattended(run("apt-get", &args))
}

/// `command` without questions from the packages' scripts.
fn unattended(command: Command) -> Command {
    Command {
        env: vec![("DEBIAN_FRONTEND".into(), "noninteractive".into())],
        display: format!("DEBIAN_FRONTEND=noninteractive {}", command.display),
        ..command
    }
}

/// The versions of those of `packages` installed.
async fn versions(
    packages: &[String],
) -> Result<std::collections::HashMap<String, String>, ToolError> {
    let mut args = vec!["-W", "-f=${Package}\t${db:Status-Abbrev}\t${Version}\n"];
    args.extend(packages.iter().map(String::as_str));
    // Exits with 1 when one of them is unknown: the others are still listed.
    let (_, said) = captured(&run("dpkg-query", &args), None, RUN_TIME).await?;
    Ok(said
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (name, status, version) = (fields.next()?, fields.next()?, fields.next()?);
            // Architecture-qualified names, such as `libc6:amd64`, are asked for without it.
            let name = name.split(':').next()?;
            status
                .starts_with("ii")
                .then(|| (name.to_owned(), version.to_owned()))
        })
        .collect())
}

/// Whether the package `name` is `installed` or `absent` now.
pub(crate) async fn state(name: &str) -> Result<String, ToolError> {
    let installed = versions(&[name.to_owned()]).await?.contains_key(name);
    Ok(if installed { "installed" } else { "absent" }.to_owned())
}

/// Every package installed now.
async fn everything_installed() -> Result<BTreeSet<String>, ToolError> {
    let query = run(
        "dpkg-query",
        &["-W", "-f=${Package}\t${db:Status-Abbrev}\n"],
    );
    let (_, said) = captured(&query, None, RUN_TIME).await?;
    Ok(said
        .lines()
        .filter_map(|line| {
            let (name, status) = line.split_once('\t')?;
            status.starts_with("ii").then(|| name.to_owned())
        })
        .collect())
}

/// Whether package lists are missing or older than [`LISTS_AGE`].
fn lists_are_old() -> bool {
    let newest = std::fs::read_dir(LISTS)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains("Packages"))
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max();
    newest
        .and_then(|newest| SystemTime::now().duration_since(newest).ok())
        .is_none_or(|age| age > LISTS_AGE)
}

/// apt's simulation as one package a line: `+name version` installed, `-name version` removed.
fn preview(said: &str) -> String {
    let lines: Vec<String> = said
        .lines()
        .filter_map(|line| {
            let (sign, rest) = match line.split_once(' ')? {
                ("Inst", rest) => ('+', rest),
                ("Remv" | "Purg", rest) => ('-', rest),
                _ => return None,
            };
            let name = rest.split_whitespace().next()?;
            // `name [old] (new release [arch])` or `name [version]`.
            let version = match (rest.find('('), rest.find('[')) {
                (Some(open), _) => rest[open + 1..].split_whitespace().next(),
                (None, Some(open)) => rest[open + 1..].split(']').next(),
                _ => None,
            };
            Some(match version {
                Some(version) => format!("{sign}{name} {version}"),
                None => format!("{sign}{name}"),
            })
        })
        .collect();
    if lines.is_empty() {
        "apt would change no package.".to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apts_simulation_becomes_one_package_a_line() {
        let said = "Reading package lists...
Inst libnginx-mod-http-geoip2 (1.24.0-2ubuntu7 Ubuntu:24.04/noble [amd64])
Inst nginx-common (1.24.0-2ubuntu7 Ubuntu:24.04/noble [all])
Inst tzdata [2024a-2] (2025b-0ubuntu0.24.04 Ubuntu:24.04/noble-updates [all])
Conf nginx-common (1.24.0-2ubuntu7 Ubuntu:24.04/noble [all])
Remv nano [7.2-2build1]
";
        assert_eq!(
            preview(said),
            "+libnginx-mod-http-geoip2 1.24.0-2ubuntu7\n+nginx-common 1.24.0-2ubuntu7\n\
             +tzdata 2025b-0ubuntu0.24.04\n-nano 7.2-2build1"
        );
        assert_eq!(
            preview("Reading package lists..."),
            "apt would change no package."
        );
    }

    #[test]
    fn package_names_are_names_and_removing_asks() {
        assert!(name("php8.3-fpm").is_ok());
        assert!(name("libstdc++6").is_ok());
        for bad in [
            "-y",
            "nginx; reboot",
            "Nginx",
            "",
            "openssh-server-",
            "nginx+",
            "libc6:i386",
        ] {
            assert!(name(bad).is_err(), "{bad}");
        }
        let removal = PackageChange {
            packages: vec!["nginx".into()],
            install: false,
            planned: None,
        };
        assert_eq!(
            command(&removal, true).display,
            "DEBIAN_FRONTEND=noninteractive apt-get remove -y nginx"
        );
        assert_eq!(removal.summary(), "Remove nginx");
    }

    #[test]
    fn an_installation_removes_only_what_its_preview_shows() {
        let install = PackageChange {
            packages: vec!["nginx".into()],
            install: true,
            planned: None,
        };
        assert!(
            command(&install, false)
                .display
                .ends_with(" --no-remove nginx")
        );
        assert!(!command(&install, true).display.contains("--no-remove"));
        assert_eq!(
            removed(
                "+nginx 1.24
-nano 7.2-2build1
-vim"
            ),
            BTreeSet::from(["nano", "vim"])
        );
        assert!(removed("apt would change no package.").is_empty());
        assert!(removed("Not known until the package lists are refreshed.").is_empty());
    }
}
