//! Web push: VAPID identification (RFC 8292), payload encryption (RFC 8291,
//! `aes128gcm`), and the subscriptions browsers register through
//! `jetmux-web`, kept in the state dir so `jetmux-notify` can send to them.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use jetmux_proto::b64;

/// Record size advertised in the `aes128gcm` header. One record holds any
/// payload jetmux sends.
const RECORD_SIZE: u32 = 4096;
/// How long a push service keeps an undelivered message.
const TTL_SECS: u64 = 24 * 60 * 60;
const JWT_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("bad key: {0}")]
    Key(String),
    #[error("encrypting: {0}")]
    Crypto(String),
    #[error("http: {0}")]
    Http(String),
    /// The push service no longer knows the subscription; drop it.
    #[error("subscription expired")]
    Gone,
}

/// A browser's `PushSubscription`, as `JSON.stringify` gives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    /// Who subscribed, from the tailnet login.
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionKeys {
    /// The browser's P-256 public key, uncompressed, base64url.
    pub p256dh: String,
    /// The 16-byte auth secret, base64url.
    pub auth: String,
}

/// The VAPID key and subscriptions, in `push.json` in the state dir.
#[derive(Debug, Serialize, Deserialize)]
pub struct Store {
    /// The VAPID private key, base64url.
    vapid: String,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

impl Store {
    pub fn path() -> PathBuf {
        jetmux_client::state_dir().join("push.json")
    }

    /// Load the store, creating it with a fresh VAPID key the first time.
    pub fn load_or_create() -> Result<Self, Error> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let store = Self {
                    vapid: b64::encode(&random_key()?.to_bytes()),
                    subscriptions: Vec::new(),
                };
                store.save()?;
                Ok(store)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Write the store, readable only by this user.
    pub fn save(&self) -> Result<(), Error> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn key(&self) -> Result<SecretKey, Error> {
        let bytes = b64::decode(&self.vapid).ok_or_else(|| Error::Key("vapid".into()))?;
        SecretKey::from_slice(&bytes).map_err(|e| Error::Key(e.to_string()))
    }

    /// The VAPID public key a browser subscribes with
    /// (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> Result<String, Error> {
        Ok(b64::encode(&self.key()?.public_key().to_sec1_bytes()))
    }

    /// Add or replace a subscription by endpoint.
    pub fn subscribe(&mut self, subscription: Subscription) {
        self.subscriptions
            .retain(|s| s.endpoint != subscription.endpoint);
        self.subscriptions.push(subscription);
    }

    pub fn unsubscribe(&mut self, endpoint: &str) {
        self.subscriptions.retain(|s| s.endpoint != endpoint);
    }

    /// Send `payload` to every subscription, dropping the ones the push
    /// service says are gone. `subject` is the VAPID contact
    /// (`mailto:...` or a URL).
    pub async fn send_all(&mut self, payload: &[u8], subject: &str) -> Result<(), Error> {
        let key = self.key()?;
        let client = client();
        let mut gone = Vec::new();
        for sub in &self.subscriptions {
            match send(&client, &key, sub, payload, subject).await {
                Ok(()) => {}
                Err(Error::Gone) => gone.push(sub.endpoint.clone()),
                Err(e) => eprintln!("jetmux-notify: push to {}: {e}", origin(&sub.endpoint)),
            }
        }
        if !gone.is_empty() {
            self.subscriptions.retain(|s| !gone.contains(&s.endpoint));
            self.save()?;
        }
        Ok(())
    }
}

type HttpsClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// An HTTP client for push services (always HTTPS) and webhooks (either).
pub fn client() -> HttpsClient {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https)
}

/// POST `body` to `url`, returning the status and response body.
pub async fn post(
    client: &HttpsClient,
    url: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), Error> {
    let mut request = hyper::Request::post(url);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| Error::Http(e.to_string()))?;
    let response = client
        .request(request)
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::Http(e.to_string()))?
        .to_bytes();
    Ok((status, body.to_vec()))
}

async fn send(
    client: &HttpsClient,
    key: &SecretKey,
    sub: &Subscription,
    payload: &[u8],
    subject: &str,
) -> Result<(), Error> {
    let body = encrypt(sub, payload, &random_key()?, &random_bytes::<16>()?)?;
    let headers = [
        ("authorization", vapid_header(key, &sub.endpoint, subject)?),
        ("content-encoding", "aes128gcm".into()),
        ("content-type", "application/octet-stream".into()),
        ("ttl", TTL_SECS.to_string()),
        ("urgency", "normal".into()),
    ];
    let (status, response) = post(client, &sub.endpoint, &headers, body).await?;
    match status {
        200..=299 => Ok(()),
        404 | 410 => Err(Error::Gone),
        _ => Err(Error::Http(format!(
            "{status}: {}",
            String::from_utf8_lossy(&response)
        ))),
    }
}

/// `scheme://host[:port]` of a URL, the audience of its VAPID token.
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    let end = url[after_scheme..]
        .find('/')
        .map_or(url.len(), |i| after_scheme + i);
    &url[..end]
}

/// `Authorization: vapid t=<jwt>, k=<public key>` for a push endpoint.
fn vapid_header(key: &SecretKey, endpoint: &str, subject: &str) -> Result<String, Error> {
    let exp = (SystemTime::now() + JWT_LIFETIME)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let header = b64::encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = serde_json::json!({ "aud": origin(endpoint), "exp": exp, "sub": subject });
    let claims = b64::encode(&serde_json::to_vec(&claims)?);
    let signing_input = format!("{header}.{claims}");
    let signer = SigningKey::from_bytes(&key.to_bytes()).map_err(|e| Error::Key(e.to_string()))?;
    let signature: Signature = signer.sign(signing_input.as_bytes());
    let public = b64::encode(&key.public_key().to_sec1_bytes());
    Ok(format!(
        "vapid t={signing_input}.{}, k={public}",
        b64::encode(&signature.to_bytes())
    ))
}

/// Encrypt `payload` for `sub` (RFC 8291), with the sender key and salt
/// passed in so tests can use the RFC's vectors.
fn encrypt(
    sub: &Subscription,
    payload: &[u8],
    sender: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, Error> {
    let bad = |what: &str| Error::Key(what.to_owned());
    let ua_public_bytes = b64::decode(&sub.keys.p256dh).ok_or_else(|| bad("p256dh"))?;
    let ua_public = PublicKey::from_sec1_bytes(&ua_public_bytes).map_err(|_| bad("p256dh"))?;
    let auth = b64::decode(&sub.keys.auth).ok_or_else(|| bad("auth"))?;
    let as_public = sender.public_key().to_sec1_bytes();
    let shared = sender.diffie_hellman(&ua_public);

    let crypto = |e: &dyn std::fmt::Display| Error::Crypto(e.to_string());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_public_bytes);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| crypto(&e))?;
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|e| crypto(&e))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|e| crypto(&e))?;

    // One record, so it's the last: payload, then the 0x02 delimiter.
    let mut record = payload.to_vec();
    record.push(2);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|e| crypto(&e))?;
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), record.as_slice())
        .map_err(|e| crypto(&e))?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + sealed.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(u8::try_from(as_public.len()).map_err(|e| crypto(&e))?);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A fresh P-256 key. Out-of-range draws are vanishingly rare; retry.
fn random_key() -> Result<SecretKey, Error> {
    loop {
        if let Ok(key) = SecretKey::from_slice(&random_bytes::<32>()?) {
            return Ok(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use p256::SecretKey;

    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};

    use crate::push::{Subscription, SubscriptionKeys, encrypt, origin, vapid_header};
    use jetmux_proto::b64;

    /// The token push services check: signed by the key it names, for the
    /// endpoint's origin.
    #[test]
    fn vapid_tokens_verify_against_their_key() {
        let key = SecretKey::from_slice(&[9; 32]).unwrap();
        let header = vapid_header(
            &key,
            "https://fcm.googleapis.com/fcm/send/x",
            "mailto:a@b.c",
        )
        .unwrap();
        let (token, public) = header
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .unwrap();
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let verifier = VerifyingKey::from_sec1_bytes(&b64::decode(public).unwrap()).unwrap();
        let signature = Signature::from_slice(&b64::decode(signature).unwrap()).unwrap();
        verifier.verify(signed.as_bytes(), &signature).unwrap();
        let claims = b64::decode(signed.split('.').nth(1).unwrap()).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:a@b.c");
    }

    /// RFC 8291 appendix A.
    #[test]
    fn encrypts_the_rfc_example() {
        let sub = Subscription {
            endpoint: "https://push.example.net/push/JzLQ3raZJfFBR0aqvOMsLrt54w4rJUsV".into(),
            keys: SubscriptionKeys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            },
            user: None,
        };
        let sender = SecretKey::from_slice(
            &b64::decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap(),
        )
        .unwrap();
        let salt: [u8; 16] = b64::decode("DGv6ra1nlYgDCS1FRnbzlw")
            .unwrap()
            .try_into()
            .unwrap();
        let body = encrypt(
            &sub,
            b"When I grow up, I want to be a watermelon",
            &sender,
            &salt,
        )
        .unwrap();
        assert_eq!(
            b64::encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
        assert_eq!(origin(&sub.endpoint), "https://push.example.net");
    }
}
