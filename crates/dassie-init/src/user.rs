//! Resolves an OCI `User` value against the rootfs `/etc/passwd` and
//! `/etc/group`.
//!
//! Accepted forms: `user`, `uid`, `user:group`, `uid:gid` and mixes of them.
//! A numeric uid with no passwd entry gets gid 0, as Docker does.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    /// Home directory from the passwd entry, if there is one.
    pub home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnknownUser(String),
    UnknownGroup(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownUser(u) => write!(f, "user {u:?} not found in /etc/passwd"),
            Self::UnknownGroup(g) => write!(f, "group {g:?} not found in /etc/group"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// Parsed `/etc/passwd` and `/etc/group`. Malformed lines are skipped.
#[derive(Debug, Default)]
pub struct Accounts {
    users: Vec<PasswdEntry>,
    groups: Vec<(String, u32)>,
}

impl Accounts {
    pub fn parse(passwd: &str, group: &str) -> Self {
        let users = passwd
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                match f.as_slice() {
                    [name, _, uid, gid, _, home, ..] => Some(PasswdEntry {
                        name: (*name).to_owned(),
                        uid: uid.parse().ok()?,
                        gid: gid.parse().ok()?,
                        home: (*home).to_owned(),
                    }),
                    _ => None,
                }
            })
            .collect();
        let groups = group
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                match f.as_slice() {
                    [name, _, gid, ..] => Some(((*name).to_owned(), gid.parse().ok()?)),
                    _ => None,
                }
            })
            .collect();
        Self { users, groups }
    }

    pub fn resolve(&self, spec: &str) -> Result<Account, Error> {
        let (user, group) = match spec.split_once(':') {
            Some((u, g)) => (u, Some(g)),
            None => (spec, None),
        };

        let entry = match user.parse::<u32>() {
            Ok(uid) => self.users.iter().find(|e| e.uid == uid),
            Err(_) => Some(
                self.users
                    .iter()
                    .find(|e| e.name == user)
                    .ok_or_else(|| Error::UnknownUser(user.to_owned()))?,
            ),
        };
        let uid = entry.map_or_else(|| user.parse().unwrap_or(0), |e| e.uid);

        let gid = match group {
            None => entry.map_or(0, |e| e.gid),
            Some(g) => match g.parse::<u32>() {
                Ok(gid) => gid,
                Err(_) => self
                    .groups
                    .iter()
                    .find(|(name, _)| name == g)
                    .map(|(_, gid)| *gid)
                    .ok_or_else(|| Error::UnknownGroup(g.to_owned()))?,
            },
        };

        Ok(Account {
            uid,
            gid,
            home: entry.map(|e| e.home.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_all_forms() {
        let accounts = Accounts::parse(
            "root:x:0:0:root:/root:/bin/sh\nbroken line\napp:x:1000:1001::/home/app:/bin/sh\n",
            "root:x:0:\nwww:x:33:app\n",
        );
        let account = |uid, gid, home: Option<&str>| Account {
            uid,
            gid,
            home: home.map(str::to_owned),
        };
        let cases = [
            ("app", Ok(account(1000, 1001, Some("/home/app")))),
            ("1000", Ok(account(1000, 1001, Some("/home/app")))),
            ("4242", Ok(account(4242, 0, None))),
            ("app:www", Ok(account(1000, 33, Some("/home/app")))),
            ("app:7", Ok(account(1000, 7, Some("/home/app")))),
            ("4242:4343", Ok(account(4242, 4343, None))),
            ("nobody", Err(Error::UnknownUser("nobody".into()))),
            ("app:nogroup", Err(Error::UnknownGroup("nogroup".into()))),
        ];
        for (spec, want) in cases {
            assert_eq!(accounts.resolve(spec), want, "{spec}");
        }
    }
}
