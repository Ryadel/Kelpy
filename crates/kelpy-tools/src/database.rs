//! Creating a MariaDB or MySQL database with a user of its own, whose password is a secret Kelpy generated
//! ([`crate::secret`]): the model only ever names it.
//!
//! Before approval Kelpy asks the server, as the administrator's account, which of the two exist: both existing, there
//! is nothing to do, and an existing user keeps its password. Once approved ([`apply`]) the statements go to the
//! client on its standard input, never on a command line other accounts can read; then Kelpy signs in as the new user
//! to check it. When that fails, what the change created is dropped.

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
            "Kelpy then signs in as the user to check it; if that fails, what this created is dropped.".to_owned(),
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
    let query = format!(
        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{}';\n\
         SELECT COUNT(*) FROM mysql.user WHERE User = '{}' AND Host = 'localhost';",
        change.name, change.user
    );
    let (code, said) = captured(&run(&client, &["-N", "-B"]), Some(&query), RUN_TIME).await?;
    if code != 0 {
        return Err(invalid(format!(
            "{client} cannot be used as this account: {}",
            said.trim()
        )));
    }
    let counts: Vec<&str> = said.split_whitespace().collect();
    let planned = DatabasePlanned {
        client,
        database_exists: counts.first() != Some(&"0"),
        user_exists: counts.get(1) != Some(&"0"),
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

/// Creates what is missing, then signs in as the user; drops what it created when that fails.
pub async fn apply(
    change: &DatabaseChange,
    planned: &DatabasePlanned,
    secrets: &Secrets,
    output: &Output,
) -> Result<(), ToolError> {
    output.set_phase("creating the database");
    let (statements, _) = secrets.fill(&change.statements(Some(planned)))?;
    let client = run(&planned.client, &[]);
    let (code, said) = captured(&client, Some(&statements), RUN_TIME).await?;
    output.text(&secrets.redact(&said));
    if code != 0 {
        return Err(ToolError::Unverified(format!(
            "{} refused the statements",
            planned.client
        )));
    }
    if !planned.database_exists {
        output.line(&format!("Created the database {}.", change.name));
    }
    if !planned.user_exists {
        output.line(&format!("Created the user {}@localhost.", change.user));
    }
    output.set_phase("checking the database");
    let (code, said) = sign_in(change, &planned.client, secrets).await?;
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
    output.set_phase("dropping what the change created");
    let mut drop = Vec::new();
    if !planned.user_exists {
        drop.push(format!("DROP USER '{}'@'localhost';", change.user));
    }
    if !planned.database_exists {
        drop.push(format!("DROP DATABASE `{}`;", change.name));
    }
    let undone = if drop.is_empty() {
        "nothing was created to drop".to_owned()
    } else {
        match captured(&client, Some(&drop.join("\n")), RUN_TIME).await {
            Ok((0, _)) => "what it created was dropped".to_owned(),
            _ => "what it created could not be dropped".to_owned(),
        }
    };
    Err(ToolError::Unverified(format!(
        "the new user cannot sign in: {undone}"
    )))
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
}
