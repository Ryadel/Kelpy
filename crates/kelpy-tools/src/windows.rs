//! Windows Server: turning features on, such as IIS, and creating an IIS site. Both run PowerShell 5.1 as the account
//! that started Kelpy, which needs administrator rights.
//!
//! Features: before approval Kelpy reads which are installed (`Get-WindowsFeature`, Windows Server only); once
//! approved it installs those missing with their management tools and checks each. A restart a feature needs is
//! reported, never made.
//!
//! An IIS site: before approval Kelpy checks that IIS is installed and that no site, application pool or binding
//! takes the name, the port and the host name; a site already there as asked needs nothing. Once approved it creates
//! the folder if missing, an application pool of the site's name (no .NET CLR, integrated pipeline), the site with its
//! binding, then checks that IIS answers there. If a step fails, it removes what it created, in reverse order.

use std::path::PathBuf;
use std::time::Duration;

use crate::executor::captured;
use crate::inspect::powershell;
use crate::{Command, Output, ToolError, invalid};

/// How long installing features may take: Windows may download them.
const INSTALL_TIME: Duration = Duration::from_secs(30 * 60);
/// How long reading the state, or creating a site, may take.
const STEP_TIME: Duration = Duration::from_secs(120);

/// Features to install: as asked, then what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureChange {
    pub features: Vec<String>,
    pub planned: Option<FeaturePlanned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeaturePlanned {
    /// One feature a line: `+Web-Server (Web Server (IIS))` to install, `=` installed already.
    pub preview: String,
    pub command: Command,
}

/// An IIS site to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteChange {
    pub name: String,
    pub path: PathBuf,
    pub port: u16,
    /// The host name it answers to; empty, every name.
    pub host: String,
    pub planned: Option<SitePlanned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePlanned {
    pub preview: String,
    pub command: Command,
}

impl FeatureChange {
    pub fn summary(&self) -> String {
        format!("Install the Windows features {}", self.features.join(", "))
    }
}

impl SiteChange {
    pub fn summary(&self) -> String {
        format!(
            "Create the IIS site {} on {}, in {}",
            self.name,
            self.binding_shown(),
            self.path.display()
        )
    }

    /// `port 80`, or `port 80 for www.example.com`.
    fn binding_shown(&self) -> String {
        if self.host.is_empty() {
            format!("port {}", self.port)
        } else {
            format!("port {} for {}", self.port, self.host)
        }
    }

    /// IIS's form of the binding: `*:80:www.example.com`.
    fn binding(&self) -> String {
        format!("*:{}:{}", self.port, self.host)
    }
}

/// A feature's name, as `Get-WindowsFeature` lists it: `Web-Server`.
pub fn feature(value: &str) -> Result<String, ToolError> {
    let fits = (1..=64).contains(&value.len())
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_'));
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(
            "a feature's name may hold only letters, digits, - and _, such as Web-Server",
        ))
    }
}

/// A site's name: letters, digits, spaces and `-_.`, at most 64.
pub fn site_name(value: &str) -> Result<String, ToolError> {
    let fits = (1..=64).contains(&value.len())
        && value.trim() == value
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, ' ' | '-' | '_' | '.'));
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(
            "`name` may hold only letters, digits, spaces, -, _ and ., at most 64",
        ))
    }
}

/// A host name for a binding, or empty for every name.
pub fn host_name(value: &str) -> Result<String, ToolError> {
    let fits = value.len() <= 253
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '.'))
        && !value.starts_with(['-', '.']);
    if fits {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(invalid(
            "`host` must be a host name, such as www.example.com",
        ))
    }
}

/// Answers `NOT-ADMIN` and ends the script without administrator rights, which reading IIS and the features needs.
const ADMIN: &str = "if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).\
IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { 'NOT-ADMIN'; exit 0 }";

/// The error for a script that answered `NOT-ADMIN`.
fn not_admin(text: &str) -> Option<ToolError> {
    text.lines().any(|line| line.trim() == "NOT-ADMIN").then(|| {
        invalid(
            "Windows lets only administrators change its features and IIS: say that Kelpy must be started from a \
             terminal opened as administrator",
        )
    })
}

/// `text` as a PowerShell string literal.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Reads which of the features are installed. Fails on a system without them, or a name Windows does not know.
pub async fn plan_features(
    change: &FeatureChange,
) -> Result<Result<FeaturePlanned, String>, ToolError> {
    let names = change
        .features
        .iter()
        .map(|name| quoted(name))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$ErrorActionPreference = 'Stop'
if (-not (Get-Command Get-WindowsFeature -ErrorAction SilentlyContinue)) {{ 'NO-SERVER'; exit 0 }}
{ADMIN}
foreach ($feature in Get-WindowsFeature -Name {names}) {{ '{{0}}|{{1}}|{{2}}' -f $feature.Name, $feature.InstallState, $feature.DisplayName }}"
    );
    let (code, text) = captured(&powershell(&script), None, STEP_TIME).await?;
    if code != 0 {
        return Err(invalid(format!(
            "Windows could not list its features: {}",
            text.trim()
        )));
    }
    let features = parse_features(&text, &change.features)?;
    let missing: Vec<&str> = features
        .iter()
        .filter(|(_, installed, _)| !installed)
        .map(|(name, _, _)| name.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(Err(format!(
            "The features {} are installed already.",
            change.features.join(", ")
        )));
    }
    let preview = features
        .iter()
        .map(|(name, installed, shown)| {
            format!("{}{name} ({shown})", if *installed { '=' } else { '+' })
        })
        .collect::<Vec<_>>()
        .join("\n");
    let names = missing
        .iter()
        .map(|name| quoted(name))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$ErrorActionPreference = 'Stop'
$result = Install-WindowsFeature -Name {names} -IncludeManagementTools
foreach ($feature in $result.FeatureResult) {{ 'Installed ' + $feature.DisplayName }}
if (-not $result.Success) {{ 'Windows reports that the installation did not succeed.'; exit 1 }}
if ($result.RestartNeeded -eq 'Yes') {{ 'A restart is needed to finish: Kelpy does not restart the server.' }}
foreach ($feature in Get-WindowsFeature -Name {names}) {{
  if ($feature.InstallState -ne 'Installed') {{ $feature.Name + ' is ' + $feature.InstallState + ', not installed.'; exit 1 }}
}}"
    );
    let mut command = powershell(&script);
    command.display = format!("Install-WindowsFeature -Name {names} -IncludeManagementTools");
    Ok(Ok(FeaturePlanned { preview, command }))
}

/// `(name, installed, display name)` of each feature asked, from `Name|InstallState|DisplayName` lines.
fn parse_features(text: &str, asked: &[String]) -> Result<Vec<(String, bool, String)>, ToolError> {
    if let Some(error) = not_admin(text) {
        return Err(error);
    }
    if text.lines().any(|line| line.trim() == "NO-SERVER") {
        return Err(invalid(
            "Windows features are installed this way on Windows Server only; on Windows 10 or 11 write a command \
             with Enable-WindowsOptionalFeature",
        ));
    }
    let listed: Vec<(String, bool, String)> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, '|');
            let name = parts.next()?.to_owned();
            let state = parts.next()?;
            Some((
                name,
                state == "Installed",
                parts.next().unwrap_or_default().to_owned(),
            ))
        })
        .collect();
    asked
        .iter()
        .map(|name| {
            listed
                .iter()
                .find(|(listed, _, _)| listed.eq_ignore_ascii_case(name))
                .cloned()
                .ok_or_else(|| {
                    invalid(format!(
                        "Windows has no feature named {name}: see Get-WindowsFeature"
                    ))
                })
        })
        .collect()
}

/// Installs the features as planned, its output going to `output`.
pub async fn apply_features(planned: &FeaturePlanned, output: &Output) -> Result<(), ToolError> {
    output.set_phase("installing the features");
    let code = crate::executor::run_local(
        &planned.command,
        None,
        Some(INSTALL_TIME),
        true,
        None,
        output,
    )
    .await?;
    if code == 0 {
        Ok(())
    } else {
        Err(invalid(
            "the features were not installed: see the output above",
        ))
    }
}

/// Checks that the site can be created; a site there as asked already needs nothing.
pub async fn plan_site(change: &SiteChange) -> Result<Result<SitePlanned, String>, ToolError> {
    let (name, binding) = (quoted(&change.name), quoted(&change.binding()));
    let script = format!(
        "$ErrorActionPreference = 'Stop'
if (-not (Get-Service W3SVC -ErrorAction SilentlyContinue)) {{ 'NO-IIS'; exit 0 }}
{ADMIN}
Import-Module WebAdministration
$site = Get-Website | Where-Object {{ $_.Name -eq {name} }}
if ($site) {{ 'SITE|' + $site.PhysicalPath + '|' + (($site.Bindings.Collection | ForEach-Object {{ $_.bindingInformation }}) -join ' '); exit 0 }}
if (Test-Path ('IIS:\\AppPools\\' + {name})) {{ 'POOL'; exit 0 }}
$taken = Get-Website | Where-Object {{ $_.Bindings.Collection | Where-Object {{ $_.bindingInformation -eq {binding} }} }}
if ($taken) {{ 'TAKEN|' + $taken[0].Name; exit 0 }}
'FREE|' + (Test-Path -LiteralPath {path})",
        path = quoted(&change.path.to_string_lossy()),
    );
    let (code, text) = captured(&powershell(&script), None, STEP_TIME).await?;
    if code != 0 {
        return Err(invalid(format!("IIS could not be read: {}", text.trim())));
    }
    if let Some(error) = not_admin(&text) {
        return Err(error);
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut parts = line.split('|');
    match parts.next() {
        Some("NO-IIS") => Err(invalid(
            "IIS is not installed: install the Windows feature Web-Server first, with install_windows_features",
        )),
        Some("POOL") => Err(invalid(format!(
            "an application pool named {} exists already: choose another name for the site",
            change.name
        ))),
        Some("TAKEN") => Err(invalid(format!(
            "the site {} answers on {} already: choose another port or host name",
            parts.next().unwrap_or_default(),
            change.binding_shown()
        ))),
        Some("SITE") => {
            let path = parts.next().unwrap_or_default();
            let bindings = parts.next().unwrap_or_default();
            let same_path = path.eq_ignore_ascii_case(&change.path.to_string_lossy());
            let same_binding = bindings
                .split(' ')
                .any(|binding| binding.eq_ignore_ascii_case(&change.binding()));
            if same_path && same_binding {
                Ok(Err(format!(
                    "The IIS site {} exists already, in {path} on {}.",
                    change.name,
                    change.binding_shown()
                )))
            } else {
                Err(invalid(format!(
                    "a site named {} exists already, in {path} with the bindings {bindings}",
                    change.name
                )))
            }
        }
        Some("FREE") => {
            let folder_exists = parts.next() == Some("True");
            Ok(Ok(planned_site(change, folder_exists)))
        }
        _ => Err(invalid(format!(
            "unexpected answer from PowerShell: {}",
            text.trim()
        ))),
    }
}

fn planned_site(change: &SiteChange, folder_exists: bool) -> SitePlanned {
    let (name, path) = (quoted(&change.name), quoted(&change.path.to_string_lossy()));
    let host = quoted(&change.host);
    let port = change.port;
    let preview = format!(
        "+ site {}\n+ binding http {}\n+ application pool {} (no .NET CLR, integrated)\n{} folder {}",
        change.name,
        change.binding(),
        change.name,
        if folder_exists { "=" } else { "+" },
        change.path.display()
    );
    let script = format!(
        "$ErrorActionPreference = 'Stop'
Import-Module WebAdministration
$created = @()
try {{
  if (-not (Test-Path -LiteralPath {path})) {{
    New-Item -ItemType Directory -Path {path} | Out-Null; $created += 'folder'; 'Created the folder ' + {path}
  }}
  New-WebAppPool -Name {name} | Out-Null; $created += 'pool'
  Set-ItemProperty ('IIS:\\AppPools\\' + {name}) -Name managedRuntimeVersion -Value ''
  'Created the application pool ' + {name}
  New-Website -Name {name} -PhysicalPath {path} -Port {port} -HostHeader {host} -ApplicationPool {name} | Out-Null
  $created += 'site'; 'Created the site ' + {name}
  $site = Get-Website | Where-Object {{ $_.Name -eq {name} }}
  if ($site.State -ne 'Started') {{ Start-Website -Name {name} }}
  $site = Get-Website | Where-Object {{ $_.Name -eq {name} }}
  if ($site.State -ne 'Started') {{ throw ('the site did not start: its state is ' + $site.State) }}
  $request = [System.Net.WebRequest]::Create('http://127.0.0.1:{port}/')
  $request.Host = $(if ({host}) {{ {host} }} else {{ 'localhost' }})
  $request.Timeout = 15000
  try {{ $response = $request.GetResponse() }} catch [System.Net.WebException] {{ $response = $_.Exception.Response }}
  if (-not $response) {{ throw 'IIS did not answer on port {port}' }}
  $server = $response.Headers['Server']
  'IIS answered on port {port}: HTTP ' + [int]$response.StatusCode + ' (' + $server + ')'
  $response.Close()
  if ($server -notmatch 'IIS') {{ throw ('port {port} answered, but not from IIS: ' + $server) }}
}} catch {{
  'The site could not be created: ' + $_.Exception.Message
  if ($created -contains 'site') {{ Remove-Website -Name {name}; 'Removed the site ' + {name} }}
  if ($created -contains 'pool') {{ Remove-WebAppPool -Name {name}; 'Removed the application pool ' + {name} }}
  if ($created -contains 'folder' -and -not (Get-ChildItem -LiteralPath {path} -Force)) {{
    Remove-Item -LiteralPath {path}; 'Removed the folder ' + {path}
  }}
  exit 1
}}"
    );
    let mut command = powershell(&script);
    command.display = format!(
        "New-WebAppPool -Name {name}\nNew-Website -Name {name} -PhysicalPath {path} -Port {port} -HostHeader {host} \
         -ApplicationPool {name}"
    );
    SitePlanned { preview, command }
}

/// Creates the site as planned, its output going to `output`; on failure, what was created is removed.
pub async fn apply_site(planned: &SitePlanned, output: &Output) -> Result<(), ToolError> {
    output.set_phase("creating the site");
    let code =
        crate::executor::run_local(&planned.command, None, Some(STEP_TIME), true, None, output)
            .await?;
    if code == 0 {
        Ok(())
    } else {
        Err(invalid(
            "the site was not created, and what was made for it was removed: see the output above",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(host: &str) -> SiteChange {
        SiteChange {
            name: "Contoso".into(),
            path: PathBuf::from(r"C:\inetpub\contoso"),
            port: 8080,
            host: host.into(),
            planned: None,
        }
    }

    #[test]
    fn names_are_checked_and_quoted_for_powershell() {
        assert!(feature("Web-Server").is_ok());
        assert!(feature("Web-Server; Remove-Item").is_err());
        assert!(site_name("Contoso site").is_ok());
        assert!(site_name("a'b").is_err());
        assert!(site_name(" padded").is_err());
        assert_eq!(host_name("WWW.Example.com").unwrap(), "www.example.com");
        assert!(host_name("bad host").is_err());
        assert_eq!(quoted(r"C:\it's"), r"'C:\it''s'");
    }

    #[test]
    fn features_are_read_from_get_windowsfeature() {
        let asked = vec!["Web-Server".to_owned(), "web-mgmt-console".to_owned()];
        let listed = "Web-Server|Available|Web Server (IIS)\r\nWeb-Mgmt-Console|Installed|IIS Management Console\r\n";
        let features = parse_features(listed, &asked).unwrap();
        assert_eq!(
            features[0],
            ("Web-Server".into(), false, "Web Server (IIS)".into())
        );
        assert!(features[1].1);
        let unknown = parse_features(listed, &["Web-Nope".into()])
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("Web-Nope"), "{unknown}");
        let client = parse_features("NO-SERVER\r\n", &asked)
            .unwrap_err()
            .to_string();
        assert!(client.contains("Enable-WindowsOptionalFeature"), "{client}");
    }

    #[test]
    fn a_site_is_shown_with_its_binding_and_what_is_created() {
        let change = site("www.contoso.com");
        assert_eq!(
            change.summary(),
            r"Create the IIS site Contoso on port 8080 for www.contoso.com, in C:\inetpub\contoso"
        );
        let planned = planned_site(&change, true);
        assert!(
            planned
                .preview
                .contains("+ binding http *:8080:www.contoso.com")
        );
        assert!(planned.preview.contains(r"= folder C:\inetpub\contoso"));
        assert!(
            planned
                .command
                .display
                .starts_with("New-WebAppPool -Name 'Contoso'")
        );
        assert!(planned_site(&site(""), false).preview.contains("+ folder"));
    }

    /// The script a command runs: PowerShell reads it from the environment.
    #[cfg(windows)]
    fn script(command: &Command) -> String {
        command.env[0].1.clone()
    }

    /// Every script Kelpy writes parses in Windows PowerShell 5.1, with no syntax error.
    #[cfg(windows)]
    #[tokio::test]
    async fn every_script_parses_in_windows_powershell() {
        let feature = FeatureChange {
            features: vec!["Web-Server".into()],
            planned: None,
        };
        let installing = FeaturePlanned {
            preview: String::new(),
            command: powershell("'placeholder'"),
        };
        let scripts = [
            script(&planned_site(&site("www.contoso.com"), false).command),
            script(&planned_site(&site(""), true).command),
            script(&installing.command),
        ];
        for text in scripts {
            let check = format!(
                "$errors = $null; [void][System.Management.Automation.Language.Parser]::ParseInput({}, [ref]$null, \
                 [ref]$errors); $errors.Count",
                quoted(&text)
            );
            let (code, said) = captured(&powershell(&check), None, STEP_TIME)
                .await
                .unwrap();
            assert_eq!((code, said.trim()), (0, "0"), "{text}");
        }
        // On a system without the features or IIS, planning says so, from scripts that ran.
        match plan_features(&feature).await {
            Ok(_) => {}
            Err(error) => {
                let said = error.to_string();
                assert!(
                    said.contains("Windows Server only") || said.contains("administrators"),
                    "{said}"
                );
            }
        }
        if let Err(error) = plan_site(&site("")).await {
            let said = error.to_string();
            assert!(
                said.contains("IIS is not installed") || said.contains("administrators"),
                "{said}"
            );
        }
    }

    /// A site created for real, on Windows Server with administrator rights: the release workflow's IIS job.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "installs IIS: run on a disposable Windows Server with `--ignored`"]
    async fn iis_is_installed_and_a_site_created_end_to_end() {
        let features = FeatureChange {
            features: vec!["Web-Server".into()],
            planned: None,
        };
        if let Ok(planned) = plan_features(&features).await.unwrap() {
            apply_features(&planned, &Output::new()).await.unwrap();
        }
        assert!(
            plan_features(&features).await.unwrap().is_err(),
            "installed now"
        );

        let folder = tempfile::tempdir().unwrap();
        let change = SiteChange {
            name: "Kelpy test".into(),
            path: folder.path().join("site"),
            port: 8088,
            host: String::new(),
            planned: None,
        };
        let planned = plan_site(&change).await.unwrap().unwrap();
        let output = Output::new();
        apply_site(&planned, &output).await.unwrap();
        let said = output.snapshot().1;
        assert!(said.contains("IIS answered on port 8088"), "{said}");
        // Asked again, there is nothing to do; another site cannot take the binding.
        assert!(plan_site(&change).await.unwrap().is_err());
        let other = SiteChange {
            name: "Kelpy other".into(),
            ..change.clone()
        };
        assert!(
            plan_site(&other)
                .await
                .unwrap_err()
                .to_string()
                .contains("answers on port 8088")
        );

        // A port another program holds: the site cannot answer there, and what was created is removed.
        let busy = std::net::TcpListener::bind("0.0.0.0:8089").unwrap();
        let blocked = SiteChange {
            name: "Kelpy blocked".into(),
            path: folder.path().join("blocked"),
            port: 8089,
            ..change.clone()
        };
        let planned = plan_site(&blocked).await.unwrap().unwrap();
        let output = Output::new();
        assert!(apply_site(&planned, &output).await.is_err());
        drop(busy);
        assert!(
            plan_site(&blocked).await.unwrap().is_ok(),
            "{}",
            output.snapshot().1
        );
        assert!(!folder.path().join("blocked").exists());

        let cleanup = powershell(
            "Import-Module WebAdministration; Remove-Website -Name 'Kelpy test'; Remove-WebAppPool -Name 'Kelpy test'",
        );
        captured(&cleanup, None, STEP_TIME).await.unwrap();
    }
}
