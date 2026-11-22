//! The TCP ports listening in each workspace (Bolt's `usePorts`): every
//! listener of this user's, keyed by its process's working directory, as
//! the row's port pill shows them. Read with `lsof` every 30 s and after a
//! kill.

use std::{collections::HashMap, process::Command};

/// Ports by the directory the listening process runs in, each list sorted
/// and without repeats (lsof lists IPv4 and IPv6 separately).
pub fn scan() -> HashMap<String, Vec<String>> {
    let Ok(out) = Command::new("lsof").args(["-iTCP", "-sTCP:LISTEN", "-n", "-P", "-FpcLn"]).output() else {
        return HashMap::new();
    };
    let user = std::env::var("USER").unwrap_or_default();
    let listeners = parse_listeners(&String::from_utf8_lossy(&out.stdout), &user);
    if listeners.is_empty() {
        return HashMap::new();
    }
    let pids: Vec<String> = listeners.iter().map(|(pid, _)| pid.to_string()).collect();
    let Ok(out) = Command::new("lsof").args(["-a", "-d", "cwd", "-Fpn", "-p", &pids.join(",")]).output() else {
        return HashMap::new();
    };
    let cwds = parse_cwds(&String::from_utf8_lossy(&out.stdout));
    let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
    for (pid, port) in listeners {
        let Some(dir) = cwds.get(&pid).filter(|d| d.as_str() != "/") else { continue };
        let ports = by_dir.entry(dir.clone()).or_default();
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    for ports in by_dir.values_mut() {
        ports.sort_by_key(|p| p.parse::<u32>().unwrap_or(u32::MAX));
    }
    by_dir
}

/// `(pid, port)` per listening socket of `user` in lsof's field output.
/// omp listens on a loopback port for its eval bridge; listing it would
/// offer a kill that ends the agent's session, so it is left out.
fn parse_listeners(out: &str, user: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let (mut pid, mut command, mut login) = (0u32, String::new(), String::new());
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => command = value.to_owned(),
            "L" => login = value.to_owned(),
            "n" if pid > 0 && login == user && command != "omp" => {
                if let Some(port) = value.rsplit(':').next().filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty()) {
                    found.push((pid, port.to_owned()));
                }
            }
            _ => {}
        }
    }
    found
}

fn parse_cwds(out: &str) -> HashMap<u32, String> {
    let mut cwds = HashMap::new();
    let mut pid = 0u32;
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "n" if pid > 0 => {
                cwds.insert(pid, value.to_owned());
            }
            _ => {}
        }
    }
    cwds
}

/// Kills whatever listens on `port` (`closePort`: `lsof -ti:<port> | xargs
/// kill -9`).
pub fn kill(port: &str) {
    let Ok(out) = Command::new("lsof").args(["-t", &format!("-i:{port}")]).output() else { return };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let _ = Command::new("kill").args(["-9", pid]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_are_this_users_minus_omp() {
        let out = "p10\ncnode\nLme\nn*:3000\nn[::1]:3000\np11\ncomp\nLme\nn127.0.0.1:4000\np12\ncvite\nLroot\nn*:5173\n";
        assert_eq!(parse_listeners(out, "me"), vec![(10, "3000".into()), (10, "3000".into())]);
        assert_eq!(parse_cwds("p10\nfcwd\nn/Users/me/app\n"), HashMap::from([(10, "/Users/me/app".into())]));
    }
}
