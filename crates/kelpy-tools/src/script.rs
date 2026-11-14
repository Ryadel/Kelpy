//! Commands the model writes, run as written in the system's shell. Kelpy cannot tell what such a command does, so
//! it is shown verbatim and approved before it runs; but with System access Full and Approval mode Auto, where it
//! runs at once unless its text looks like it could lose data, lock the administrator out or stop the system
//! ([`risk`]).

use std::path::PathBuf;

use crate::executor::MAX_OUTPUT;
use crate::inspect::powershell;
use crate::{Command, Os, Work};

/// The most a command may hold. Encoded for PowerShell it stays within Windows' limit for a command line.
pub const MAX_SCRIPT: usize = 8 << 10;

/// `text` run in `directory` by the shell of `os`: Windows PowerShell; else bash, or sh where bash is missing.
pub fn script(os: Os, text: &str, directory: PathBuf) -> Work {
    let shells = match os {
        Os::Windows => vec![powershell(text)],
        Os::Linux | Os::MacOs => ["bash", "sh"]
            .into_iter()
            .map(|shell| Command {
                program: shell.to_owned(),
                args: vec!["-c".to_owned(), text.to_owned()],
                env: Vec::new(),
                display: text.to_owned(),
            })
            .collect(),
    };
    Work::Script { shells, directory }
}

/// What the model is told of `run_command` on `os`.
pub fn description(os: Os) -> String {
    let (shell, unattended) = match os {
        Os::Windows => (
            "Windows PowerShell 5.1",
            "-Confirm:$false, -Force where it is safe",
        ),
        Os::Linux | Os::MacOs => (
            "bash (sh where bash is missing)",
            "-y, --no-pager, DEBIAN_FRONTEND=noninteractive",
        ),
    };
    format!(
        "Runs a command you write in {shell}, as the account that started Kelpy, for what no other tool does. The \
         administrator sees it verbatim, with its folder, and approves it before it runs, but with System access Full \
         and Approval mode Auto, where it runs at once unless it looks like it deletes files or databases, formats \
         disks, restarts the computer, removes packages, or changes the firewall, network, SSH, sudo, logins, accounts \
         or boot. Each command starts in a new shell in `directory`: a `cd` does not carry over to the next one. \
         Nothing can be typed into it, so use options that do not ask ({unattended}). Programs it starts end with it: \
         start lasting ones as services. It has no time limit, and the administrator can stop it; its output is kept \
         up to {} KiB.",
        MAX_OUTPUT >> 10,
    )
}

const DELETES: &str = "It deletes files or folders: what is deleted may not come back.";
const DATABASE: &str = "It drops or empties a database or its tables: the data may not come back.";
const DISKS: &str = "It formats, partitions or overwrites a disk: what is on it may not come back.";
const POWER: &str = "It restarts or shuts down the computer: what runs on it stops.";
const WAY_IN: &str = "It changes the firewall, the network, SSH, the sudo or login rules, or the accounts: a mistake \
                      could lock you out.";
const BOOT: &str = "It changes how the system starts: a mistake could keep it from starting.";
const PACKAGES: &str = "It removes packages or Windows features: the programs stop working and their data may go with them.";
const HIDDEN: &str = "It runs code it downloads, decodes or builds as it goes, which cannot be read here before it runs.";

/// The files whose change could lock the administrator out, as a command names them: those of a file's change
/// (see [`crate::change`]), and the accounts' own.
const WAY_IN_FILES: &[&str] = &[
    "/etc/ssh",
    "sshd_config",
    "authorized_keys",
    "/etc/sudoers",
    "/etc/pam.d",
    "/etc/security",
    "/etc/ufw",
    "/etc/firewalld",
    "/etc/nftables",
    "/etc/iptables",
    "/etc/hosts.allow",
    "/etc/hosts.deny",
    "/etc/network",
    "/etc/netplan",
    "/etc/networkmanager",
    "/etc/sysconfig/network-scripts",
    "/etc/resolv.conf",
    "/etc/passwd",
    "/etc/shadow",
    "/etc/group",
    "/etc/pf.conf",
    "/etc/pf.anchors",
];
/// The files whose change could keep the system from starting.
const BOOT_FILES: &[&str] = &["/etc/fstab", "/etc/crypttab", "/etc/default/grub", "/boot/"];

/// Why a command the model wrote waits for approval even in Auto, with System access Full: it looks like it could
/// lose data, lock the administrator out or stop the system, by the floor's measure (deleting files or databases,
/// disks, restarts, the firewall, network, SSH, sudo, logins and accounts, the boot, removing packages, code that
/// cannot be read before it runs). Found by patterns on its text: a command can do the same in ways no pattern sees,
/// so this spares the administrator questions about ordinary commands, and is no guarantee.
pub fn risk(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    let flat = format!(
        " {} ",
        lower.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let found = |needles: &[&str]| needles.iter().any(|needle| flat.contains(needle));
    if found(&[
        "drop database",
        "drop table",
        "drop schema",
        "truncate table",
        "delete from",
        "flushall",
        "flushdb",
        "dropdatabase(",
    ]) {
        return Some(DATABASE);
    }
    if found(&[
        " -encodedcommand ",
        " -enc ",
        " -ec ",
        "frombase64string",
        "base64 -d",
        "base64 --decode",
        "downloadstring(",
    ]) {
        return Some(HIDDEN);
    }
    commands(text).find_map(|command| command.risk())
}

/// A simple command of a command line, in lower case: its name, without its folder or `.exe`; its arguments,
/// without quotes; and its text.
struct Simple {
    name: String,
    args: Vec<String>,
    text: String,
}

/// Each simple command in `text`. Commands are split at `; | & ( ) { }`, new lines and backquotes, quoted or not,
/// so that each command of a pipeline, a list, a substitution or a quoted command line is seen.
fn commands(text: &str) -> impl Iterator<Item = Simple> {
    text.split([';', '|', '&', '(', ')', '{', '}', '\n', '\r', '`'])
        .filter_map(|text| {
            let words: Vec<String> = text
                .split_whitespace()
                .map(|word| word.trim_matches(['\'', '"', ',']).to_owned())
                .filter(|word| !word.is_empty())
                .collect();
            simple(&words, text)
        })
}

/// The command `words` of `text` run in the end: what a wrapper such as `sudo -u root` runs, or a shell such as
/// `bash -c`, `cmd /c` or `powershell -Command`, is seen in its place. The words keep their case until then: `sudo -H`
/// is not `sudo -h`.
fn simple(mut words: &[String], text: &str) -> Option<Simple> {
    loop {
        let (first, rest) = words.split_first()?;
        // A variable set for the command.
        if first.contains('=') && !first.starts_with('$') {
            words = rest;
            continue;
        }
        let name = first
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(first)
            .to_lowercase();
        let name = name.trim_end_matches(".exe");
        if let Some(command) = command_line(name, rest) {
            words = command;
        } else if let Some(wrapper) = WRAPPERS.iter().find(|wrapper| wrapper.name == name) {
            words = wrapper.command(rest);
        } else {
            return Some(Simple {
                name: name.to_owned(),
                args: rest.iter().map(|arg| arg.to_lowercase()).collect(),
                text: text.to_lowercase(),
            });
        }
    }
}

/// A program that runs the command written after its own options and arguments.
struct Wrapper {
    name: &'static str,
    /// Its options that take a value written apart, as `-u root`, in their case: `-H` and `-h` differ.
    values: &'static [&'static str],
    /// How many arguments it takes before the command, as `timeout`'s duration.
    before: usize,
}

const fn wrapper(name: &'static str, values: &'static [&'static str], before: usize) -> Wrapper {
    Wrapper {
        name,
        values,
        before,
    }
}

const WRAPPERS: &[Wrapper] = &[
    wrapper(
        "sudo",
        &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-C",
            "-D",
            "-R",
            "-r",
            "-T",
            "-t",
            "-U",
            "--user",
            "--group",
            "--host",
            "--prompt",
            "--close-from",
            "--chdir",
            "--role",
            "--type",
            "--other-user",
            "--command-timeout",
            "--chroot",
        ],
        0,
    ),
    wrapper("doas", &["-u", "-C"], 0),
    wrapper("pkexec", &["--user"], 0),
    wrapper(
        "runuser",
        &[
            "-u",
            "-g",
            "-G",
            "-s",
            "-w",
            "--user",
            "--group",
            "--supp-group",
            "--shell",
            "--whitelist-environment",
        ],
        0,
    ),
    wrapper("env", &["-u", "-C", "--unset", "--chdir"], 0),
    wrapper("nice", &["-n", "--adjustment"], 0),
    wrapper(
        "ionice",
        &[
            "-c",
            "-n",
            "-p",
            "-P",
            "-u",
            "--class",
            "--classdata",
            "--pid",
            "--pgid",
            "--uid",
        ],
        0,
    ),
    wrapper("timeout", &["-s", "-k", "--signal", "--kill-after"], 1),
    // `-i`, `-l` and `-e` take their value only joined to them, as `-i{}`.
    wrapper(
        "xargs",
        &[
            "-I",
            "-n",
            "-P",
            "-L",
            "-a",
            "-d",
            "-E",
            "-s",
            "--max-args",
            "--max-procs",
            "--max-lines",
            "--arg-file",
            "--delimiter",
            "--max-chars",
            "--eof",
        ],
        0,
    ),
    wrapper("time", &["-f", "-o", "--format", "--output"], 0),
    wrapper("watch", &["-n", "--interval"], 0),
    wrapper("stdbuf", &["-i", "-o", "-e"], 0),
    wrapper(
        "flock",
        &["-w", "-E", "--wait", "--timeout", "--conflict-exit-code"],
        1,
    ),
    wrapper("chroot", &[], 1),
    wrapper("taskset", &[], 1),
    wrapper("exec", &["-a"], 0),
    wrapper("nohup", &[], 0),
    wrapper("setsid", &[], 0),
    wrapper("command", &[], 0),
    wrapper("builtin", &[], 0),
    wrapper("busybox", &[], 0),
    wrapper("call", &[], 0),
    wrapper(".", &[], 0),
];

impl Wrapper {
    /// The command in `args`, written after this wrapper.
    fn command<'w>(&self, mut args: &'w [String]) -> &'w [String] {
        let (mut before, mut options) = (self.before, true);
        while let Some((arg, rest)) = args.split_first() {
            if options && arg == "--" {
                options = false;
                args = rest;
            } else if options && arg.len() > 1 && arg.starts_with('-') {
                args = match self.values.contains(&arg.as_str()) {
                    true => rest.get(1..).unwrap_or(&[]),
                    false => rest,
                };
            } else if before > 0 {
                before -= 1;
                args = rest;
            } else {
                break;
            }
        }
        args
    }
}

/// PowerShell's options that take a value, by their names; any start of three letters or more is the same option.
const POWERSHELL_VALUES: &[&str] = &[
    "-executionpolicy",
    "-windowstyle",
    "-workingdirectory",
    "-version",
    "-outputformat",
    "-inputformat",
    "-psconsolefile",
    "-configurationname",
    "-settingsfile",
    "-custompipename",
    "-encodedarguments",
];
/// Their short names.
const POWERSHELL_SHORT_VALUES: &[&str] = &["-ex", "-ep", "-w", "-wd", "-v", "-o", "-of", "-if"];

/// The command line `args` holds for the shell or launcher `name` to run, if it is one: `bash -c`, `su -c`,
/// `cmd /c`, `powershell -Command` (or its first argument), `runas`. The sh family's options keep their case
/// (`-C` is not `-c`); those of `cmd` and PowerShell have none.
fn command_line<'w>(name: &str, args: &'w [String]) -> Option<&'w [String]> {
    match name {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" | "fish" | "su" | "runuser" => {
            // `-c`, alone or after other one-letter options as in `-lc`; su's `--command`.
            let at = args.iter().position(|arg| {
                arg == "--command"
                    || (arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('c'))
            })?;
            Some(&args[at + 1..])
        }
        "cmd" => {
            let run = |arg: &str| {
                ["/c", "/k", "/r"]
                    .iter()
                    .any(|run| arg.eq_ignore_ascii_case(run))
            };
            let at = args
                .iter()
                .position(|arg| run(arg) || !arg.starts_with('/'))?;
            run(&args[at]).then(|| &args[at + 1..])
        }
        "runas" => {
            let at = args.iter().position(|arg| !arg.starts_with('/'))?;
            Some(&args[at..])
        }
        "powershell" | "pwsh" => {
            let mut rest = args;
            while let Some((arg, after)) = rest.split_first() {
                let arg = arg.to_lowercase();
                if !arg.starts_with('-') {
                    // Windows PowerShell's first argument is a command; pwsh's, a script.
                    return Some(rest);
                }
                if arg.len() > 1 && ("-commandwithargs".starts_with(arg.as_str()) || arg == "-cwa")
                {
                    return Some(after);
                }
                if arg.len() > 1 && "-file".starts_with(arg.as_str()) {
                    return None;
                }
                let value = POWERSHELL_SHORT_VALUES.contains(&arg.as_str())
                    || (arg.len() > 3
                        && POWERSHELL_VALUES
                            .iter()
                            .any(|option| option.starts_with(arg.as_str())));
                rest = match value {
                    true => after.get(1..).unwrap_or(&[]),
                    false => after,
                };
            }
            None
        }
        _ => None,
    }
}

/// PowerShell commands that change the firewall or the network, by the start of their names.
const NETWORK_CMDLETS: &[&str] = &[
    "set-netfirewall",
    "new-netfirewall",
    "remove-netfirewall",
    "disable-netfirewall",
    "enable-netfirewall",
    "set-netipaddress",
    "new-netipaddress",
    "remove-netipaddress",
    "set-netipinterface",
    "disable-netadapter",
    "restart-netadapter",
    "set-netadapter",
    "rename-netadapter",
    "set-dnsclientserveraddress",
    "new-netroute",
    "set-netroute",
    "remove-netroute",
    "set-netconnectionprofile",
];

impl Simple {
    fn has(&self, options: &[&str]) -> bool {
        self.args.iter().any(|arg| options.contains(&arg.as_str()))
    }

    fn any(&self, test: impl Fn(&str) -> bool) -> bool {
        self.args.iter().any(|arg| test(arg))
    }

    /// Whether it names a service that keeps the way into the server open, such as sshd or WinRM.
    fn names_way_in(&self) -> bool {
        self.any(|arg| {
            let base = arg.trim_end_matches(".service").trim_end_matches(".socket");
            crate::service::ACCESS.contains(&base)
        })
    }

    /// Whether it may write a file: a redirection, or a command that writes or moves files.
    fn writes(&self) -> bool {
        self.text.contains('>')
            || matches!(
                self.name.as_str(),
                "tee"
                    | "cp"
                    | "mv"
                    | "install"
                    | "ln"
                    | "chmod"
                    | "chown"
                    | "chattr"
                    | "truncate"
                    | "dd"
                    | "set-content"
                    | "add-content"
                    | "out-file"
                    | "copy-item"
                    | "move-item"
                    | "new-item"
                    | "set-acl"
                    | "icacls"
                    | "setfacl"
                    | "visudo"
                    | "crontab"
            )
            || (matches!(self.name.as_str(), "sed" | "perl")
                && self.any(|arg| arg.starts_with("-i")))
    }

    fn mentions(&self, files: &[&str]) -> bool {
        files.iter().any(|file| self.text.contains(file))
    }

    fn risk(&self) -> Option<&'static str> {
        let first = self.args.first().map(String::as_str).unwrap_or("");
        let found = match self.name.as_str() {
            "rm" | "rmdir" | "rd" | "del" | "erase" | "unlink" | "shred" | "srm"
            | "remove-item" | "ri" | "clear-content" | "clc" | "wipe" => Some(DELETES),
            "find" if self.has(&["-delete", "rm"]) => Some(DELETES),
            "git" if first == "clean" => Some(DELETES),
            "rsync" if self.any(|arg| arg.starts_with("--delete")) => Some(DELETES),
            "robocopy" if self.has(&["/mir", "/purge"]) => Some(DELETES),
            "dropdb" => Some(DATABASE),
            "mysqladmin" if self.has(&["drop"]) => Some(DATABASE),
            "format" | "format-volume" | "clear-disk" | "initialize-disk" | "remove-partition"
            | "diskpart" | "fdisk" | "sfdisk" | "gdisk" | "sgdisk" | "parted" | "wipefs"
            | "blkdiscard" | "mkswap" | "mke2fs" => Some(DISKS),
            name if name.starts_with("mkfs") => Some(DISKS),
            "dd" if self
                .any(|arg| arg.starts_with("of=/dev/") || arg.starts_with("of=\\\\.\\")) =>
            {
                Some(DISKS)
            }
            "reboot" | "shutdown" | "poweroff" | "halt" | "kexec" | "restart-computer"
            | "stop-computer" => Some(POWER),
            "init" | "telinit" if self.has(&["0", "6"]) => Some(POWER),
            "systemctl"
                if self.has(&[
                    "reboot",
                    "poweroff",
                    "halt",
                    "kexec",
                    "rescue",
                    "emergency",
                    "isolate",
                ]) =>
            {
                Some(POWER)
            }
            "systemctl"
                if self.has(&["stop", "restart", "disable", "mask", "kill"])
                    && self.names_way_in() =>
            {
                Some(WAY_IN)
            }
            "service" if self.has(&["stop", "restart"]) && self.names_way_in() => Some(WAY_IN),
            "stop-service" | "restart-service" | "set-service" | "suspend-service"
                if self.names_way_in() =>
            {
                Some(WAY_IN)
            }
            "sc" | "net"
                if self.has(&["stop", "config", "delete", "pause"]) && self.names_way_in() =>
            {
                Some(WAY_IN)
            }
            "net" if matches!(first, "user" | "localgroup") && self.args.len() > 2 => Some(WAY_IN),
            // Listing the rules or the settings changes nothing: anything else may.
            "iptables" | "ip6tables" if !self.has(&["-l", "--list", "--list-rules"]) => {
                Some(WAY_IN)
            }
            "nft" if first != "list" => Some(WAY_IN),
            "ufw" if !matches!(first, "status" | "show" | "app" | "version") => Some(WAY_IN),
            "firewall-cmd"
                if !self.args.iter().all(|arg| {
                    ["--list", "--get", "--state", "--query", "--info", "--zone"]
                        .iter()
                        .any(|reading| arg.starts_with(reading))
                }) =>
            {
                Some(WAY_IN)
            }
            "pfctl" if first != "-s" => Some(WAY_IN),
            "netsh" if self.has(&["set", "add", "delete", "del", "reset", "dump"]) => Some(WAY_IN),
            "route" if self.has(&["add", "delete", "del", "change", "-f", "flush"]) => Some(WAY_IN),
            "ifconfig"
                if self.has(&[
                    "down", "up", "del", "delete", "add", "inet", "netmask", "mtu",
                ]) =>
            {
                Some(WAY_IN)
            }
            "socketfilterfw" if self.any(|arg| !arg.starts_with("--get")) => Some(WAY_IN),
            "networksetup"
                if self.any(|arg| {
                    arg.starts_with('-') && !arg.starts_with("-get") && !arg.starts_with("-list")
                }) =>
            {
                Some(WAY_IN)
            }
            "iptables-restore" | "ip6tables-restore" | "ifdown" => Some(WAY_IN),
            "ip" if self.has(&[
                "del", "delete", "flush", "down", "set", "add", "change", "replace",
            ]) =>
            {
                Some(WAY_IN)
            }
            "nmcli"
                if self.has(&[
                    "down",
                    "delete",
                    "del",
                    "modify",
                    "mod",
                    "off",
                    "disconnect",
                ]) =>
            {
                Some(WAY_IN)
            }
            "netplan" if self.has(&["apply", "try"]) => Some(WAY_IN),
            "passwd"
            | "chpasswd"
            | "usermod"
            | "userdel"
            | "deluser"
            | "gpasswd"
            | "remove-localuser"
            | "disable-localuser"
            | "set-localuser"
            | "remove-localgroupmember"
            | "remove-aduser"
            | "disable-adaccount"
            | "set-adaccountpassword" => Some(WAY_IN),
            "chage" if !self.has(&["-l", "--list"]) => Some(WAY_IN),
            "dscl"
                if self.any(|arg| {
                    matches!(
                        arg,
                        "-create" | "-delete" | "-passwd" | "-append" | "-change"
                    )
                }) =>
            {
                Some(WAY_IN)
            }
            name if NETWORK_CMDLETS
                .iter()
                .any(|cmdlet| name.starts_with(cmdlet)) =>
            {
                Some(WAY_IN)
            }
            "bcdedit" | "bcdboot" | "grub-install" | "update-grub" | "grub-mkconfig"
            | "grub2-mkconfig" | "efibootmgr" | "update-initramfs" | "dracut" => Some(BOOT),
            "apt" | "apt-get" | "aptitude" if self.has(&["remove", "purge", "autoremove"]) => {
                Some(PACKAGES)
            }
            "yum" | "dnf" | "zypper" | "microdnf"
                if self.has(&["remove", "erase", "autoremove", "rm"]) =>
            {
                Some(PACKAGES)
            }
            "pacman" if self.any(|arg| arg.starts_with("-r")) => Some(PACKAGES),
            "dpkg" if self.has(&["-r", "-p", "--remove", "--purge"]) => Some(PACKAGES),
            "rpm" if self.any(|arg| arg.starts_with("-e")) => Some(PACKAGES),
            "snap" | "brew" | "choco" | "winget" | "scoop"
                if self.has(&["remove", "uninstall", "rm"]) =>
            {
                Some(PACKAGES)
            }
            "uninstall-windowsfeature"
            | "remove-windowsfeature"
            | "disable-windowsoptionalfeature"
            | "uninstall-package"
            | "remove-appxpackage"
            | "remove-windowscapability" => Some(PACKAGES),
            "iex" | "invoke-expression" | "eval" => Some(HIDDEN),
            // A shell reading what is piped into it, as `curl … | sh`.
            "sh" | "bash" | "zsh" | "dash" | "ksh"
                if self.args.iter().all(|arg| arg.starts_with('-')) =>
            {
                Some(HIDDEN)
            }
            _ => None,
        };
        found
            .or_else(|| (self.writes() && self.mentions(WAY_IN_FILES)).then_some(WAY_IN))
            .or_else(|| (self.writes() && self.mentions(BOOT_FILES)).then_some(BOOT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_commands_run_and_those_that_could_lose_data_or_lock_you_out_ask() {
        for ordinary in [
            "Get-Service W3SVC",
            "Get-ChildItem C:\\inetpub -Recurse | Select-Object FullName",
            "systemctl status nginx",
            "systemctl restart nginx",
            "sudo systemctl reload nginx",
            "apt-get install -y nginx",
            "cat /etc/ssh/sshd_config",
            "grep -E 'error|warn' /var/log/syslog | tail -n 50",
            "mkdir -p /var/www/blog && cp index.html /var/www/blog/",
            "New-Item -ItemType Directory C:\\sites\\blog",
            "ip addr show",
            "docker ps -a",
            "Get-NetFirewallRule -Enabled True",
            "tar czf /root/backup.tgz /var/www",
            "echo $PATH > /tmp/path.txt",
            "netstat -tlnp",
            "mysql -e 'SELECT user, host FROM mysql.user'",
            "sed -e 's/a/b/' notes.txt",
            "Get-Process | Sort-Object CPU -Descending | Select-Object -First 5",
            "sudo iptables -L -n -v",
            "ufw status verbose",
            "nft list ruleset",
            "firewall-cmd --list-all",
            "route print",
            "netsh interface ipv4 show addresses",
            "chage -l bob",
        ] {
            assert_eq!(risk(ordinary), None, "{ordinary}");
        }
        for (risky, why) in [
            ("rm -rf /var/www/old", DELETES),
            ("cd /tmp; sudo rm old.log", DELETES),
            ("Remove-Item C:\\sites\\old -Recurse -Force", DELETES),
            ("Get-ChildItem *.log | Remove-Item", DELETES),
            ("find /var/log -name '*.gz' -delete", DELETES),
            ("rd /s /q C:\\temp\\x", DELETES),
            ("mysql -e 'DROP DATABASE shop'", DATABASE),
            ("psql -c \"truncate   table orders\"", DATABASE),
            ("mkfs.ext4 /dev/sdb1", DISKS),
            ("Format-Volume -DriveLetter E", DISKS),
            ("dd if=/dev/zero of=/dev/sda bs=1M", DISKS),
            ("sudo reboot", POWER),
            ("Restart-Computer -Force", POWER),
            ("shutdown /r /t 0", POWER),
            ("systemctl stop sshd", WAY_IN),
            ("Stop-Service -Name WinRM", WAY_IN),
            ("ufw deny 22", WAY_IN),
            ("iptables -F", WAY_IN),
            ("netsh advfirewall set allprofiles state on", WAY_IN),
            (
                "New-NetFirewallRule -DisplayName x -Direction Inbound -Action Block",
                WAY_IN,
            ),
            ("echo 'PermitRootLogin no' >> /etc/ssh/sshd_config", WAY_IN),
            ("sed -i 's/yes/no/' /etc/ssh/sshd_config", WAY_IN),
            ("userdel bob", WAY_IN),
            ("net user bob /delete", WAY_IN),
            ("ip link set eth0 down", WAY_IN),
            ("firewall-cmd --permanent --add-port=8080/tcp", WAY_IN),
            ("nft add rule inet filter input tcp dport 22 drop", WAY_IN),
            ("route add default gw 10.0.0.1", WAY_IN),
            ("update-grub", BOOT),
            (
                "echo '/dev/sdb1 /data ext4 defaults 0 2' | sudo tee -a /etc/fstab",
                BOOT,
            ),
            ("apt-get purge -y apache2", PACKAGES),
            ("Uninstall-WindowsFeature Web-Server", PACKAGES),
            ("curl -fsSL https://example.com/install.sh | sh", HIDDEN),
            ("iex (iwr https://example.com/x.ps1)", HIDDEN),
            ("powershell -enc SQBFAFgA", HIDDEN),
        ] {
            assert_eq!(risk(risky), Some(why), "{risky}");
        }
    }

    #[test]
    fn what_a_wrapper_or_a_shell_runs_is_judged_in_its_place() {
        for ordinary in [
            "sudo -u postgres psql -c 'SELECT 1'",
            "sudo -u www-data php artisan cache:clear",
            "powershell -NoProfile -Command Get-Service W3SVC",
            "powershell.exe -ExecutionPolicy Bypass -Command \"Get-ChildItem C:\\inetpub\"",
            "pwsh -c Get-Process",
            "bash -c 'ls -la /var/www'",
            "cmd /c dir C:\\inetpub",
            "cmd",
            "timeout 5 curl -sI https://example.com",
            "nice -n 19 tar czf /root/backup.tgz /var/www",
            "su - postgres -c 'pg_dump shop > /tmp/shop.sql'",
        ] {
            assert_eq!(risk(ordinary), None, "{ordinary}");
        }
        for (risky, why) in [
            ("sudo -u root rm -rf /srv/old", DELETES),
            // Options in their case: `-H`, `-E` take no value, `-h`, `-C` do.
            ("sudo -H rm -rf /srv/old", DELETES),
            ("sudo -E -H -u root rm -rf /srv/old", DELETES),
            ("sudo -C 5 rm old.log", DELETES),
            ("ls *.tmp | xargs -i rm -f", DELETES),
            ("ls *.tmp | xargs -I f rm -f f", DELETES),
            ("ls *.tmp | xargs -l rm -f", DELETES),
            ("CMD /C DEL C:\\temp\\old.log", DELETES),
            ("POWERSHELL -COMMAND Remove-Item C:\\old", DELETES),
            ("Sudo -H Rm -rf /srv/old", DELETES),
            ("sudo -u root -g root -- rm old.log", DELETES),
            ("sudo --user root rm old.log", DELETES),
            ("doas -u root rm old.log", DELETES),
            ("cmd /c del /q C:\\temp\\old.log", DELETES),
            ("cmd.exe /q /c rd /s /q C:\\temp\\x", DELETES),
            ("C:\\Windows\\System32\\cmd.exe /k erase x.txt", DELETES),
            (
                "powershell -Command \"Remove-Item C:\\sites\\old -Recurse\"",
                DELETES,
            ),
            (
                "powershell -NoProfile -ExecutionPolicy Bypass -c Remove-Item x",
                DELETES,
            ),
            ("powershell Remove-Item C:\\old", DELETES),
            ("pwsh -nop -Command \"Stop-Service WinRM\"", WAY_IN),
            (
                "powershell -WindowStyle Hidden -Command Restart-Computer",
                POWER,
            ),
            ("bash -c 'systemctl stop sshd'", WAY_IN),
            ("sudo sh -lc \"rm -f /tmp/x\"", DELETES),
            ("su -c 'userdel bob' root", WAY_IN),
            ("su - root -c reboot", POWER),
            ("runuser -u www-data -- rm -rf /var/www/cache", DELETES),
            ("nice -n 10 rm big.log", DELETES),
            ("timeout -s KILL 30 rm big.log", DELETES),
            ("env -u HOME LANG=C rm big.log", DELETES),
            (
                "find . -name '*.tmp' -print0 | xargs -0 -n 10 rm -f",
                DELETES,
            ),
            (
                "runas /user:Administrator \"cmd /c del C:\\x.txt\"",
                DELETES,
            ),
            ("sudo bash -c \"cmd=x; sudo -u root rm -rf /srv\"", DELETES),
        ] {
            assert_eq!(risk(risky), Some(why), "{risky}");
        }
    }
}
