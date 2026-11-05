//! Creating a MariaDB or MySQL database with a user of its own, whose password is a secret Kelpy generated
//! ([`crate::secret`]): the model only ever names it.
//!
//! Before approval Kelpy asks the server, as the administrator's account, which of the two exist: both existing, there
//! is nothing to do, and an existing user keeps its password. Once approved ([`apply`]) the statements go to the
//! client on its standard input, never on a command line other accounts can read; then Kelpy signs in as the new user
//! to check it. When a statement or that check fails, Kelpy asks again what exists and drops what the change
//! created: each statement commits on its own, so a failure can leave the database without its user.

use crate::executor::{RUN_TIME, captured};
use crate::inspect::run;
use crate::secret::{self, Secrets};
use crate::{Command, Output, ToolError, invalid};

/// A database and its user to create: as asked, then with what exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseChange {
    pub name: String,
    pub user: String,
    /// The password's placeholder, `{{secret:name}}`.
    pub password: String,
    pub planned: Option<DatabasePlanned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabasePlanned {
    /// `mariadb` or `mysql`.
    pub client: String,
    pub database_exists: bool,
    pub user_exists: bool,
}

impl DatabaseChange {
    pub fn summary(&self) -> String {
        format!(
            "Create the database {} for the user {}",
            self.name, self.user
        )
    }

    /// The statements, as shown: with the password's placeholder.
    pub fn statements(&self, planned: Option<&DatabasePlanned>) -> String {
        let (name, user) = (&self.name, &self.user);
        let mut lines = Vec::new();
        if !planned.is_some_and(|planned| planned.database_exists) {
            lines.push(format!(
                "CREATE DATABASE `{name}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"
            ));
        }
        if !planned.is_some_and(|planned| planned.user_exists) {
            lines.push(format!(
                "CREATE USER '{user}'@'localhost' IDENTIFIED BY '{}';",
                self.password
            ));
        }
        lines.push(format!(
            "GRANT ALL PRIVILEGES ON `{name}`.* TO '{user}'@'localhost';"
        ));
        lines.push("FLUSH PRIVILEGES;".to_owned());
        lines.join("\n")
    }

    pub fn notes(&self, planned: &DatabasePlanned) -> Vec<String> {
        let mut notes = vec![format!(
            "Run by {} as this account, the statements on its input. The password is the secret {}: Kelpy puts it \
             in, and the AI model never sees it.",
            planned.client, self.password
        )];
        if planned.user_exists {
            notes.push(format!(
                "The user {} exists already: it keeps its password.",
                self.user
            ));
        }
        notes.push(
            "Kelpy then signs in as the user to check it; if a step fails, what this created is dropped.".to_owned(),
        );
        notes
    }
}

/// A database's or a user's name: letters, digits and _ only, so that it needs no quoting.
pub fn identifier(value: &str, field: &str) -> Result<String, ToolError> {
    let fits = (1..=32).contains(&value.len())
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '_');
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(format!(
            "`{field}` may hold only letters, digits and _, at most 32"
        )))
    }
}

/// The password as the model gives it: the placeholder of one secret, nothing else.
pub fn password(value: &str) -> Result<String, ToolError> {
    let name = value
        .strip_prefix("{{secret:")
        .and_then(|rest| rest.strip_suffix("}}"))
        .ok_or_else(|| {
            invalid(
                "`password` must be a secret's placeholder, such as {{secret:wordpress_db}}: create the secret \
                 with generate_secret",
            )
        })?;
    Ok(secret::placeholder(&secret::name(name)?))
}

/// Finds the client and what exists. `Ok(Err(text))` when both exist already.
pub async fn plan(
    change: &DatabaseChange,
    secrets: &Secrets,
    installed: impl Fn(&str) -> bool,
) -> Result<Result<DatabasePlanned, String>, ToolError> {
    // Its secret must exist now: it is put in only once approved.
    secrets.fill(&change.password)?;
    let client = ["mariadb", "mysql"]
        .into_iter()
        .find(|client| installed(client))
        .ok_or_else(|| {
            invalid("MariaDB or MySQL is not installed here: install it first, such as the package mariadb-server")
        })?
        .to_owned();
    let (database_exists, user_exists) = existing(change, &client).await.map_err(invalid)?;
    let planned = DatabasePlanned {
        client,
        database_exists,
        user_exists,
    };
    if planned.database_exists && planned.user_exists {
        // The configuration written with the secret works only if the user signs in with it.
        if sign_in(change, &planned.client, secrets).await?.0 != 0 {
            return Err(invalid(format!(
                "the user {} exists with another password, and Kelpy does not change it: choose another user name",
                change.user
            )));
        }
        return Ok(Err(format!(
            "The database {} and the user {} exist already: nothing to change. An existing user keeps its \
             password.",
            change.name, change.user
        )));
    }
    Ok(Ok(planned))
}

/// Creates what is missing, then signs in as the user; drops what it created when a step fails.
pub async fn apply(
    change: &DatabaseChange,
    planned: &DatabasePlanned,
    secrets: &Secrets,
    output: &Output,
) -> Result<(), ToolError> {
    // What exists decides what the statements create, and what a failure drops: it must be as shown.
    let now = existing(change, &planned.client)
        .await
        .map_err(ToolError::Unverified)?;
    if now != (planned.database_exists, planned.user_exists) {
        return Err(ToolError::Unverified(format!(
            "the database {} or the user {} was created or dropped after the change was shown: nothing was done, \
             plan it again",
            change.name, change.user
        )));
    }
    let Err(problem) = create(change, planned, secrets, output).await else {
        return Ok(());
    };
    output.set_phase("dropping what the change created");
    let undone = drop_created(change, planned).await;
    Err(ToolError::Unverified(format!("{problem}: {undone}")))
}

/// Runs the statements and signs in as the user; on failure, what went wrong.
async fn create(
    change: &DatabaseChange,
    planned: &DatabasePlanned,
    secrets: &Secrets,
    output: &Output,
) -> Result<(), String> {
    output.set_phase("creating the database");
    let (statements, _) = secrets
        .fill(&change.statements(Some(planned)))
        .map_err(|error| error.to_string())?;
    let (code, said) = captured(&run(&planned.client, &[]), Some(&statements), RUN_TIME)
        .await
        .map_err(|error| error.to_string())?;
    output.text(&secrets.redact(&said));
    if code != 0 {
        return Err(format!("{} refused the statements", planned.client));
    }
    if !planned.database_exists {
        output.line(&format!("Created the database {}.", change.name));
    }
    if !planned.user_exists {
        output.line(&format!("Created the user {}@localhost.", change.user));
    }
    output.set_phase("checking the database");
    let (code, said) = sign_in(change, &planned.client, secrets)
        .await
        .map_err(|error| error.to_string())?;
    if code == 0 {
        output.line(&format!(
            "Signed in as {} to {}: it works.",
            change.user, change.name
        ));
        return Ok(());
    }
    output.line(&format!(
        "Signing in as {} failed: {}",
        change.user,
        secrets.redact(said.trim())
    ));
    Err("the new user cannot sign in".to_owned())
}

/// Drops what exists now and did not before the change; says what came of it.
async fn drop_created(change: &DatabaseChange, planned: &DatabasePlanned) -> String {
    let (database, user) = match existing(change, &planned.client).await {
        Ok(now) => now,
        Err(error) => {
            return format!("what it created could not be checked, nor dropped ({error})");
        }
    };
    let mut drop = Vec::new();
    if user && !planned.user_exists {
        drop.push(format!("DROP USER '{}'@'localhost';", change.user));
    }
    if database && !planned.database_exists {
        drop.push(format!("DROP DATABASE `{}`;", change.name));
    }
    if drop.is_empty() {
        return "it created nothing to drop".to_owned();
    }
    let client = run(&planned.client, &[]);
    let dropped = matches!(
        captured(&client, Some(&drop.join("\n")), RUN_TIME).await,
        Ok((0, _))
    );
    format!(
        "what it created {} ({})",
        if dropped {
            "was dropped"
        } else {
            "could not be dropped"
        },
        drop.join(" ")
    )
}

/// Whether the database and the user exist, asked as the administrator's account.
async fn existing(change: &DatabaseChange, client: &str) -> Result<(bool, bool), String> {
    let query = format!(
        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{}';\n\
         SELECT COUNT(*) FROM mysql.user WHERE User = '{}' AND Host = 'localhost';",
        change.name, change.user
    );
    let (code, said) = captured(&run(client, &["-N", "-B"]), Some(&query), RUN_TIME)
        .await
        .map_err(|error| error.to_string())?;
    if code != 0 {
        return Err(format!(
            "{client} cannot be used as this account: {}",
            said.trim()
        ));
    }
    counts(&said).ok_or_else(|| format!("{client} gave an unexpected answer: {}", said.trim()))
}

/// The two counts of [`existing`]'s query, as whether each exists.
fn counts(said: &str) -> Option<(bool, bool)> {
    let counts: Vec<u64> = said
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match counts[..] {
        [database, user] => Some((database > 0, user > 0)),
        _ => None,
    }
}

/// Signs in as the user, with the secret, to the database: the exit code and what the client said.
async fn sign_in(
    change: &DatabaseChange,
    client: &str,
    secrets: &Secrets,
) -> Result<(i32, String), ToolError> {
    let (password, _) = secrets.fill(&change.password)?;
    let sign_in = Command {
        env: vec![("MYSQL_PWD".into(), password)],
        ..run(
            client,
            &[
                "-u",
                &change.user,
                "-h",
                "localhost",
                "-N",
                "-B",
                &change.name,
            ],
        )
    };
    captured(&sign_in, Some("SELECT 1;"), RUN_TIME).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_need_no_quoting_and_the_password_is_a_secret() {
        assert!(identifier("wordpress_1", "name").is_ok());
        for bad in ["word press", "a`b", "x'--", ""] {
            assert!(identifier(bad, "name").is_err(), "{bad}");
        }
        assert_eq!(password("{{secret:wp_db}}").unwrap(), "{{secret:wp_db}}");
        assert!(password("hunter2").is_err());
        assert!(password("{{secret:a b}}").is_err());

        let change = DatabaseChange {
            name: "wordpress".into(),
            user: "wp".into(),
            password: "{{secret:wp_db}}".into(),
            planned: None,
        };
        let statements = change.statements(None);
        assert!(statements.contains("CREATE DATABASE `wordpress`"));
        assert!(statements.contains("IDENTIFIED BY '{{secret:wp_db}}'"));
        let existing = DatabasePlanned {
            client: "mariadb".into(),
            database_exists: false,
            user_exists: true,
        };
        assert!(!change.statements(Some(&existing)).contains("CREATE USER"));
    }

    /// A fake client: creating the database succeeds and commits, creating the user then fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failure_after_the_database_was_created_drops_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let created = dir.path().join("created");
        let client = dir.path().join("mariadb");
        std::fs::write(
            &client,
            format!(
                "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n\
                 *'SELECT COUNT'*) if [ -e '{0}' ]; then echo 1; else echo 0; fi; echo 0 ;;\n\
                 *'DROP DATABASE'*) rm '{0}' ;;\n\
                 *'CREATE DATABASE'*) touch '{0}'; echo 'ERROR 1396 at line 2: Operation CREATE USER failed'; \
                 exit 1 ;;\nesac\n",
                created.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
        let secrets = Secrets::new();
        secrets.generate("wp_db").unwrap();
        let change = DatabaseChange {
            name: "wordpress".into(),
            user: "wp".into(),
            password: "{{secret:wp_db}}".into(),
            planned: None,
        };
        let planned = DatabasePlanned {
            client: client.to_string_lossy().into_owned(),
            database_exists: false,
            user_exists: false,
        };

        let error = apply(&change, &planned, &secrets, &Output::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refused the statements")
                && error.contains("was dropped (DROP DATABASE `wordpress`;)"),
            "{error}"
        );
        assert!(!created.exists());
    }

    #[test]
    fn what_exists_is_read_strictly() {
        assert_eq!(counts("1\n0\n"), Some((true, false)));
        assert_eq!(counts("0\t0"), Some((false, false)));
        for unexpected in ["", "1", "1\n0\n1", "ERROR 1045"] {
            assert_eq!(counts(unexpected), None, "{unexpected}");
        }
    }
}
