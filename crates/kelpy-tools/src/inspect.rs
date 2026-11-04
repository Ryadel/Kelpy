//! The commands of each inspection, for each kind of system. All of them only read, and none needs elevated
//! rights to run, though some show more with them (the processes behind ports on Linux, for one).
//!
//! On Windows they read the registry, .NET and fast commands rather than WMI (CIM), which can take tens of seconds
//! to answer.

use crate::{Command, Os, Work};

/// A program with its arguments.
pub(crate) fn run(program: &str, args: &[&str]) -> Command {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let display = std::iter::once(program.to_owned())
        .chain(args.iter().map(|arg| shell_quoted(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    Command {
        program: program.to_owned(),
        args,
        env: Vec::new(),
        display,
    }
}

/// The variable that carries a PowerShell script to [`powershell`]'s fixed command line.
const SCRIPT_VARIABLE: &str = "KELPY_COMMAND";

/// A PowerShell script, shown as typed. It runs with UTF-8 output, so that names in any language come through, and
/// without progress bars.
///
/// The script reaches PowerShell in an environment variable, not on its command line: there PowerShell would need its
/// quotes escaped, and it would put the whole command line, Kelpy's lines included, before each error. Run as a script
/// block, its errors show its own lines.
pub(crate) fn powershell(script: &str) -> Command {
    Command {
        program: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            format!(
                "[Console]::OutputEncoding = [Text.Encoding]::UTF8; $ProgressPreference = 'SilentlyContinue'; \
                 . ([ScriptBlock]::Create($env:{SCRIPT_VARIABLE}))"
            ),
        ],
        env: vec![(SCRIPT_VARIABLE.to_owned(), script.to_owned())],
        display: script.to_owned(),
    }
}

/// One command per step, without alternatives.
fn steps(commands: Vec<Command>) -> Work {
    Work::Commands(commands.into_iter().map(|command| vec![command]).collect())
}

/// `value` as a POSIX shell would need it, for display only: commands never go through a shell.
fn shell_quoted(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|char| char.is_alphanumeric() || "-_./=:,%@+".contains(char));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// `value` as a PowerShell string literal.
pub(crate) fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A table PowerShell does not cut to the console's width.
const TABLE: &str = "Out-String -Width 400";

pub fn system_info(os: Os) -> Work {
    match os {
        Os::Linux => steps(vec![
            run("cat", &["/etc/os-release"]),
            run("uname", &["-srm"]),
            run("uptime", &[]),
            run("free", &["-h"]),
            run("nproc", &[]),
        ]),
        // The registry still names Windows 11 "Windows 10": its build tells them apart. The performance counter
        // runs from the start of the system.
        Os::Windows => steps(vec![powershell(
            "$os = Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion'; $name = $os.ProductName; \
             if ([int]$os.CurrentBuild -ge 22000) { $name = $name -replace 'Windows 10', 'Windows 11' }; \
             Add-Type -AssemblyName Microsoft.VisualBasic; $memory = New-Object Microsoft.VisualBasic.Devices.ComputerInfo; \
             [pscustomobject][ordered]@{ System = $name; Version = $os.DisplayVersion; \
             Build = \"$($os.CurrentBuild).$($os.UBR)\"; Computer = [Environment]::MachineName; \
             Architecture = $env:PROCESSOR_ARCHITECTURE; Processors = [Environment]::ProcessorCount; \
             MemoryGB = [math]::Round($memory.TotalPhysicalMemory / 1GB, 1); \
             FreeMemoryGB = [math]::Round($memory.AvailablePhysicalMemory / 1GB, 1); \
             UptimeHours = [math]::Round([Diagnostics.Stopwatch]::GetTimestamp() / [Diagnostics.Stopwatch]::Frequency / 3600, 1) } \
             | Format-List",
        )]),
        Os::MacOs => steps(vec![
            run("sw_vers", &[]),
            run("uname", &["-srm"]),
            run("uptime", &[]),
            run("sysctl", &["hw.ncpu", "hw.memsize"]),
        ]),
    }
}

pub fn services(os: Os, filter: Option<&str>) -> Work {
    let pattern = filter.map(|filter| format!("*{filter}*"));
    match os {
        Os::Linux => {
            let mut systemctl = vec![
                "list-units",
                "--type=service",
                "--all",
                "--no-pager",
                "--plain",
            ];
            systemctl.extend(pattern.as_deref());
            Work::Commands(vec![vec![
                run("systemctl", &systemctl),
                run("rc-status", &["--all"]),
                run("service", &["--status-all"]),
            ]])
        }
        Os::Windows => {
            let matching = match &pattern {
                Some(pattern) => format!(
                    " | Where-Object {{ $_.Name -like {p} -or $_.DisplayName -like {p} }}",
                    p = quoted(pattern)
                ),
                None => String::new(),
            };
            steps(vec![powershell(&format!(
                "Get-Service{matching} | Sort-Object Name | Format-Table -AutoSize Name,Status,StartType,DisplayName \
                 | {TABLE}"
            ))])
        }
        Os::MacOs => steps(vec![run("launchctl", &["list"])]),
    }
}

pub fn service(os: Os, name: &str) -> Work {
    match os {
        Os::Linux => Work::Commands(vec![vec![
            run("systemctl", &["status", name, "--no-pager", "--lines=20"]),
            run("rc-service", &[name, "status"]),
            run("service", &[name, "status"]),
        ]]),
        Os::Windows => {
            let name = quoted(name);
            steps(vec![powershell(&format!(
                "$service = Get-Service -Name {name} -ErrorAction SilentlyContinue | Select-Object -First 1; \
                 if (-not $service) {{ $service = Get-Service -DisplayName {name} -ErrorAction SilentlyContinue \
                 | Select-Object -First 1 }}; if ($service) {{ $service | Format-List Name,DisplayName,Status,StartType,\
                 ServiceType,DependentServices,ServicesDependedOn; Get-ItemProperty \
                 ('HKLM:\\SYSTEM\\CurrentControlSet\\Services\\' + $service.Name) | Format-List ImagePath,ObjectName,Description; \
                 sc.exe queryex $service.Name }} else {{ \"No service is named \" + {name} + \".\" }}"
            ))])
        }
        Os::MacOs => steps(vec![run("launchctl", &["list", name])]),
    }
}

pub fn packages(os: Os, filter: Option<&str>) -> Work {
    let pattern = filter.map(|filter| format!("*{filter}*"));
    match os {
        Os::Linux => {
            let with = |args: &[&str]| -> Vec<String> {
                args.iter()
                    .map(|arg| (*arg).to_owned())
                    .chain(pattern.clone())
                    .collect()
            };
            let owned = |program: &str, args: Vec<String>| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(program, &args)
            };
            let pacman = match filter {
                Some(filter) => run("pacman", &["-Qs", filter]),
                None => run("pacman", &["-Q"]),
            };
            Work::Commands(vec![vec![
                owned(
                    "dpkg-query",
                    with(&["-W", "-f", "${Package} ${Version}\\n"]),
                ),
                owned("rpm", with(&["-qa"])),
                owned("apk", with(&["list", "--installed"])),
                pacman,
            ]])
        }
        Os::Windows => {
            let (programs, features) = match &pattern {
                Some(pattern) => {
                    let p = quoted(pattern);
                    (
                        format!(" -and $_.DisplayName -like {p}"),
                        format!(" | Where-Object {{ $_.Name -like {p} -or $_.Caption -like {p} }}"),
                    )
                }
                None => (String::new(), String::new()),
            };
            steps(vec![powershell(&format!(
                "'Installed programs:'; Get-ItemProperty \
                 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
                 'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
                 'HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' -ErrorAction SilentlyContinue \
                 | Where-Object {{ $_.DisplayName{programs} }} | Sort-Object DisplayName -Unique \
                 | Format-Table -AutoSize DisplayName,DisplayVersion,Publisher | {TABLE}; \
                 'Windows features turned on:'; Get-CimInstance Win32_OptionalFeature -Filter 'InstallState=1'\
                 {features} | Sort-Object Name | Format-Table -AutoSize Name,Caption | {TABLE}"
            ))])
        }
        Os::MacOs => steps(vec![run("brew", &["list", "--versions"])]),
    }
}

pub fn ports(os: Os) -> Work {
    match os {
        Os::Linux => Work::Commands(vec![vec![
            run("ss", &["-tulpn"]),
            run("netstat", &["-tulpn"]),
        ]]),
        Os::Windows => steps(vec![powershell(
            "$names = @{}; Get-Process | ForEach-Object { $names[$_.Id] = $_.ProcessName }; \
             '{0,-5} {1,-40} {2,7} {3}' -f 'Proto', 'Local address', 'PID', 'Process'; \
             netstat -ano | ForEach-Object { $f = -split $_; \
             if (($f[0] -eq 'TCP' -and $f[3] -eq 'LISTENING') -or $f[0] -eq 'UDP') { \
             '{0,-5} {1,-40} {2,7} {3}' -f $f[0], $f[1], $f[-1], $names[[int]$f[-1]] } }",
        )]),
        Os::MacOs => steps(vec![run("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN"])]),
    }
}

pub fn disks(os: Os) -> Work {
    match os {
        Os::Linux => steps(vec![run(
            "df",
            &[
                "-hT", "-x", "tmpfs", "-x", "devtmpfs", "-x", "squashfs", "-x", "overlay",
            ],
        )]),
        // Network drives can take seconds each to measure, or hang: they are only named.
        Os::Windows => steps(vec![powershell(&format!(
            "$drives = [IO.DriveInfo]::GetDrives(); $drives | Where-Object {{ $_.DriveType -ne 'Network' -and $_.IsReady }} \
             | Format-Table -AutoSize Name,DriveType,DriveFormat,@{{n='SizeGB';e={{[math]::Round($_.TotalSize/1GB,1)}}}},\
             @{{n='FreeGB';e={{[math]::Round($_.AvailableFreeSpace/1GB,1)}}}},VolumeLabel | {TABLE}; \
             $network = $drives | Where-Object DriveType -eq 'Network'; \
             if ($network) {{ 'Network drives, not measured: ' + ($network.Name -join ', ') }}"
        ))]),
        Os::MacOs => steps(vec![run("df", &["-h"])]),
    }
}

pub fn processes(os: Os) -> Work {
    match os {
        Os::Linux => steps(vec![run(
            "ps",
            &["-eo", "pid,user,%cpu,%mem,rss,etime,comm", "--sort=-%cpu"],
        )]),
        Os::Windows => steps(vec![powershell(&format!(
            "Get-Process | Sort-Object CPU -Descending | Select-Object -First 60 Id,ProcessName,\
             @{{n='CPUSeconds';e={{[math]::Round($_.CPU,1)}}}},@{{n='MemoryMB';e={{[math]::Round($_.WorkingSet64/1MB)}}}} \
             | Format-Table -AutoSize | {TABLE}"
        ))]),
        Os::MacOs => steps(vec![run(
            "ps",
            &["-Ao", "pid,user,%cpu,%mem,rss,etime,comm", "-r"],
        )]),
    }
}

/// The latest `lines` log entries: of `unit` on Linux, of `log` (and `source`) on Windows.
pub fn logs(os: Os, lines: u64, unit: Option<&str>, log: &str, source: Option<&str>) -> Work {
    let count = lines.to_string();
    match os {
        Os::Linux => {
            let mut args = vec!["--no-pager", "-n", count.as_str()];
            if let Some(unit) = unit {
                args.extend(["-u", unit]);
            }
            steps(vec![run("journalctl", &args)])
        }
        Os::Windows => {
            let filter = match source {
                Some(source) => format!(
                    "-FilterHashtable @{{LogName={}; ProviderName={}}}",
                    quoted(log),
                    quoted(source)
                ),
                None => format!("-LogName {}", quoted(log)),
            };
            steps(vec![powershell(&format!(
                "Get-WinEvent {filter} -MaxEvents {lines} -ErrorAction SilentlyContinue \
                 | Format-Table -AutoSize -Wrap TimeCreated,Id,LevelDisplayName,ProviderName,Message | {TABLE}"
            ))])
        }
        Os::MacOs => {
            // About a line a second on a quiet Mac: the minutes asked for follow from the lines.
            let minutes = format!("{}m", lines.div_ceil(60).max(1));
            steps(vec![run(
                "log",
                &["show", "--last", &minutes, "--style", "compact"],
            )])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_shown_as_they_would_be_typed() {
        let Work::Commands(steps) = packages(Os::Linux, Some("nginx")) else {
            panic!();
        };
        assert_eq!(
            steps[0][0].display,
            r"dpkg-query -W -f '${Package} ${Version}\n' '*nginx*'"
        );
        assert_eq!(steps[0][0].args.last().unwrap(), "*nginx*");
        // Arguments go to the program as they are: the quotes are for display only.
        assert_eq!(steps[0][0].args[2], r"${Package} ${Version}\n");
        assert_eq!(shell_quoted("it's"), r"'it'\''s'");

        let Work::Commands(steps) = services(Os::Windows, Some("w3")) else {
            panic!();
        };
        let command = &steps[0][0];
        assert_eq!(command.program, "powershell.exe");
        assert!(
            command
                .display
                .starts_with("Get-Service | Where-Object { $_.Name -like '*w3*'")
        );
        assert!(!command.display.contains("OutputEncoding"));
        assert!(command.args[3].starts_with("[Console]::OutputEncoding"));
        assert_eq!(
            command.env,
            [(SCRIPT_VARIABLE.to_owned(), command.display.clone())]
        );
    }
}
