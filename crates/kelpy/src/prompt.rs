//! Plain terminal questions. Only asked when both stdin and stdout are terminals.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Asks a yes/no question; an empty answer picks `default`.
pub fn confirm(question: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        match read_line(&format!("{question} {hint} "))?
            .to_ascii_lowercase()
            .as_str()
        {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

/// Asks to pick one of `count` numbered options; an empty answer picks `default`. Both are 1-based.
pub fn choose(count: usize, default: usize) -> io::Result<usize> {
    loop {
        let answer = read_line(&format!("Choice [{default}]: "))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.parse::<usize>() {
            Ok(choice) if (1..=count).contains(&choice) => return Ok(choice),
            _ => println!("Please enter a number from 1 to {count}."),
        }
    }
}

/// Asks for a folder and returns it as an absolute path.
pub fn ask_path(question: &str) -> io::Result<PathBuf> {
    loop {
        let answer = read_line(question)?;
        // "Copy as path" on Windows wraps the path in quotes.
        let answer = answer.trim_matches('"');
        if !answer.is_empty() {
            return std::path::absolute(answer);
        }
    }
}

/// Asks for a line of text; an empty answer picks `default`.
pub fn ask(question: &str, default: &str) -> io::Result<String> {
    let hint = if default.is_empty() {
        String::new()
    } else {
        format!(" [{default}]")
    };
    let answer = read_line(&format!("{question}{hint}: "))?;
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer
    })
}

/// Asks for a secret without showing what is typed.
pub fn secret(question: &str) -> io::Result<String> {
    Ok(rpassword::prompt_password(format!("{question}: "))?
        .trim()
        .to_owned())
}

pub fn read_line(question: &str) -> io::Result<String> {
    print!("{question}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "input closed"));
    }
    Ok(line.trim().to_owned())
}
