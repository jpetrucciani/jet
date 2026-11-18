//! What an `ssh` command line connects to, for telling where a program in
//! front really is.

/// Who and where an `ssh` in front is connected to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ssh {
    pub user: Option<String>,
    pub host: String,
}

/// The destination of an `ssh` command line, if it's one: `ssh -p 22
/// me@box`, `ssh.exe -l me box`, `"C:\...\ssh.exe" box ls`.
pub fn ssh_target(argv: &[String]) -> Option<Ssh> {
    ssh_destination(argv).map(|(_, ssh)| ssh)
}

/// Where in `argv`, an `ssh` command line, its destination is, and what
/// it names.
pub fn ssh_destination(argv: &[String]) -> Option<(usize, Ssh)> {
    let (program, args) = argv.split_first()?;
    let name = program.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if name != "ssh" && name != "ssh.exe" {
        return None;
    }
    /// Options that take a value, from ssh(1).
    const VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut user = None;
    let mut args = args.iter().enumerate();
    while let Some((i, arg)) = args.next() {
        let Some(flags) = arg.strip_prefix('-') else {
            let (u, host) = match arg.rsplit_once('@') {
                Some((u, host)) => (Some(u.to_owned()), host),
                None => (user, arg.as_str()),
            };
            // ssh://user@host:port
            let host = host.strip_prefix("ssh://").unwrap_or(host);
            let ssh = Ssh {
                user: u,
                host: host.to_owned(),
            };
            return Some((i + 1, ssh));
        };
        // Flags group (`-tt`); a valued one takes the rest or the next arg.
        for (i, flag) in flags.char_indices() {
            if VALUED.contains(flag) {
                let rest = flags.get(i + 1..).unwrap_or_default();
                let value = if rest.is_empty() {
                    args.next().map(|(_, v)| v.clone())
                } else {
                    Some(rest.to_owned())
                };
                if flag == 'l' {
                    user = value;
                }
                break;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::ssh::{Ssh, ssh_target};

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn ssh_destinations() {
        let ssh = |line: &str| ssh_target(&argv(line));
        let me_box = Some(Ssh {
            user: Some("me".into()),
            host: "box".into(),
        });
        assert_eq!(ssh("ssh me@box"), me_box);
        assert_eq!(ssh("ssh.exe -p 2222 -A me@box ls"), me_box);
        assert_eq!(ssh("C:\\OpenSSH\\SSH.EXE -l me -tt box"), me_box);
        assert_eq!(ssh("ssh -lme box"), me_box);
        assert_eq!(ssh("ssh -i key box").map(|s| s.user), Some(None));
        assert_eq!(ssh("sshd -D"), None);
        assert_eq!(ssh("ssh -V"), None);
    }
}
