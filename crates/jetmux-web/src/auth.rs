//! Who is asking, and what they may do. `tailscale serve` puts the tailnet
//! login in `Tailscale-User-Login` on every request it proxies; the config
//! maps logins to roles. Share links carry a signed, expiring token instead
//! and only ever see one session, read-only.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use hmac::{Hmac, KeyInit, Mac};
use jetmux_proto::b64;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Set by `tailscale serve` from the requester's tailnet identity.
const LOGIN_HEADER: &str = "tailscale-user-login";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Watches; can switch its own tab and focus, nothing else.
    Viewer,
    /// Types and runs actions in sessions.
    Pair,
    /// Everything: starting sessions from profiles, share links.
    Owner,
}

#[derive(Clone, Debug)]
pub struct Access {
    pub user: Option<String>,
    pub role: Role,
    /// Set for share links: the one session they may see.
    pub session: Option<String>,
}

impl Access {
    pub fn sees(&self, session: &str) -> bool {
        self.session.as_deref().is_none_or(|s| s == session)
    }
}

/// `[web.users]`: login (or `"*"` for anyone else on the tailnet) to role,
/// and `local`: the role for requests with no tailnet identity, which only
/// come from this machine. Unset means those are refused.
pub struct Policy<'a> {
    pub users: &'a BTreeMap<String, Role>,
    pub local: Option<Role>,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    /// The session.
    s: String,
    /// Expiry, in Unix seconds.
    e: u64,
}

/// Signs share links. The key lives in the state dir, so links survive
/// restarts; deleting the file revokes them all.
pub struct Signer {
    key: [u8; 32],
}

impl Signer {
    pub fn load_or_create() -> std::io::Result<Self> {
        let path = jetmux_client::state_dir().join("web-secret");
        let mut key = [0u8; 32];
        match std::fs::File::open(&path) {
            Ok(mut file) => file.read_exact(&mut key)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?
                    .write_all(&key)?;
            }
            Err(e) => return Err(e),
        }
        Ok(Self { key })
    }

    /// HMAC takes keys of any length, so this is always `Some`.
    fn mac(&self, payload: &[u8]) -> Option<Hmac<Sha256>> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key).ok()?;
        mac.update(payload);
        Some(mac)
    }

    /// A token for read-only access to `session` for `ttl_secs`.
    pub fn share(&self, session: &str, ttl_secs: u64) -> Option<String> {
        let claims = Claims {
            s: session.to_owned(),
            e: now().saturating_add(ttl_secs),
        };
        let payload = b64::encode(&serde_json::to_vec(&claims).ok()?);
        let tag = self.mac(payload.as_bytes())?.finalize().into_bytes();
        Some(format!("{payload}.{}", b64::encode(&tag)))
    }

    /// The session a still-valid token is for.
    pub fn verify(&self, token: &str) -> Option<String> {
        let (payload, tag) = token.split_once('.')?;
        self.mac(payload.as_bytes())?
            .verify_slice(&b64::decode(tag)?)
            .ok()?;
        let claims: Claims = serde_json::from_slice(&b64::decode(payload)?).ok()?;
        (claims.e > now()).then_some(claims.s)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The access a request has, if any.
pub fn access(
    policy: &Policy,
    signer: &Signer,
    headers: &HeaderMap,
    share: Option<&str>,
) -> Option<Access> {
    if let Some(token) = share {
        return signer.verify(token).map(|session| Access {
            user: None,
            role: Role::Viewer,
            session: Some(session),
        });
    }
    match headers.get(LOGIN_HEADER).and_then(|v| v.to_str().ok()) {
        Some(login) => {
            let role = policy.users.get(login).or_else(|| policy.users.get("*"))?;
            Some(Access {
                user: Some(login.to_owned()),
                role: *role,
                session: None,
            })
        }
        None => policy.local.map(|role| Access {
            user: None,
            role,
            session: None,
        }),
    }
}

/// Requests that change something must carry `X-Jetmux: 1`. A page on
/// another site can't add that header without a CORS preflight, which this
/// server never answers, so it can't use the browser's tailnet identity
/// against us.
pub fn csrf_ok(headers: &HeaderMap) -> bool {
    headers.get("x-jetmux").is_some_and(|v| v == "1")
}

/// A WebSocket upgrade must come from a page served here.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    match (host, origin) {
        (Some(host), Some(origin)) => origin
            .split_once("://")
            .is_some_and(|(_, rest)| rest == host),
        // Non-browser clients send no Origin; they can't ride a browser's
        // identity either.
        (_, None) => true,
        (None, Some(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::HeaderMap;

    use crate::auth::{Policy, Role, Signer, access, same_origin};

    #[test]
    fn logins_map_to_roles_and_share_links_to_one_session() {
        let signer = Signer { key: [7; 32] };
        let users = BTreeMap::from([
            ("me@github".to_owned(), Role::Owner),
            ("*".to_owned(), Role::Viewer),
        ]);
        let policy = Policy {
            users: &users,
            local: None,
        };
        let mut headers = HeaderMap::new();
        assert!(access(&policy, &signer, &headers, None).is_none());
        headers.insert("tailscale-user-login", "me@github".parse().unwrap());
        assert_eq!(
            access(&policy, &signer, &headers, None).unwrap().role,
            Role::Owner
        );
        headers.insert("tailscale-user-login", "friend@github".parse().unwrap());
        assert_eq!(
            access(&policy, &signer, &headers, None).unwrap().role,
            Role::Viewer
        );

        let token = signer.share("dev", 60).unwrap();
        let shared = access(&policy, &signer, &HeaderMap::new(), Some(&token)).unwrap();
        assert_eq!(
            (shared.role, shared.session.as_deref()),
            (Role::Viewer, Some("dev"))
        );
        assert!(shared.sees("dev") && !shared.sees("other"));
        // Another session's claims under this token's signature.
        let (_, tag) = token.split_once('.').unwrap();
        let forged = jetmux_proto::b64::encode(br#"{"s":"ops","e":99999999999}"#);
        assert!(signer.verify(&format!("{forged}.{tag}")).is_none());
        assert!(signer.verify(&signer.share("dev", 0).unwrap()).is_none());
    }

    #[test]
    fn websockets_must_come_from_this_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "jetmux.tail.ts.net".parse().unwrap());
        headers.insert("origin", "https://jetmux.tail.ts.net".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!same_origin(&headers));
    }
}
