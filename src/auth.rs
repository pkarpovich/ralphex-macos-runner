//! The runner's access tokens from the authorization server.
//!
//! The runner is a confidential client of its own: [`TokenSource`] asks the
//! token endpoint for a `farm:run` token with the `client_credentials` grant,
//! authenticates with a fresh RS256 client assertion signed by the
//! [`SigningKey`] each time, keeps the token in memory and asks for a new one
//! [`TOKEN_RENEW_MARGIN`] before it expires. No error this module returns
//! carries the token, the assertion or key material: a refusal names the HTTP
//! status and the OAuth `error` code only.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::protocol::client::Sleeper;
use crate::protocol::types::{RunnerName, TOKEN_RENEW_MARGIN, TOKEN_TIMEOUT};

/// The one scope a runner asks for.
pub const SCOPE: &str = "farm:run";

/// The grant a runner uses.
pub const GRANT_TYPE: &str = "client_credentials";

/// The `client_assertion_type` of a `private_key_jwt` client.
pub const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// The time between a client assertion's `iat` and its `exp`.
pub const ASSERTION_LIFETIME: Duration = Duration::from_secs(60);

const JTI_BYTES: usize = 16;
const MAX_TOKEN_BODY_BYTES: usize = 65536;

/// Who the runner is at the authorization server and what it asks a token for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The authorization server's issuer, the audience of every assertion.
    pub issuer: String,
    /// The URL the token request is posted to.
    pub token_endpoint: String,
    /// The runner's client id, which is also its name at the farm.
    pub client_id: RunnerName,
    /// The `kid` the authorization server knows the runner's public key by.
    pub key_id: String,
    /// The farm's identifier, sent as the token's `resource`.
    pub resource: String,
}

/// Why the key file could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The file could not be read.
    #[error("the key file {path} could not be read: {message}")]
    Read {
        /// The path that was tried.
        path: String,
        /// The reason the read failed.
        message: String,
    },
    /// The path names something other than a regular file.
    #[error("the key file {path} is not a regular file")]
    NotAFile {
        /// The path that was tried.
        path: String,
    },
    /// Users other than the owner can read the file.
    #[error(
        "the key file {path} is readable by others (mode {mode:04o}); run chmod 600 on it and create a new key, since this one may have leaked"
    )]
    Shared {
        /// The path that was tried.
        path: String,
        /// The file's permission bits.
        mode: u32,
    },
    /// The file does not hold an RSA private key in PEM.
    #[error("the key file {path} does not hold an RSA private key in PEM")]
    NotRsa {
        /// The path that was tried.
        path: String,
    },
}

/// Why no access token could be had.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The token endpoint could not be reached or did not answer in time.
    #[error("the token endpoint could not be reached: {0}")]
    Transport(String),
    /// The token endpoint refused the request.
    #[error("{}", refusal(*.status, .code.as_deref()))]
    Refused {
        /// The HTTP status of the answer.
        status: u16,
        /// The OAuth `error` code of the answer, when it carried one.
        code: Option<String>,
    },
    /// The token endpoint answered without a usable token.
    #[error("the token endpoint answered without a usable access token")]
    Unusable,
    /// The client assertion could not be signed.
    #[error("the client assertion could not be signed")]
    Assertion,
}

fn refusal(status: u16, code: Option<&str>) -> String {
    match code {
        Some(code) => format!("the token endpoint answered {status} ({code})"),
        None => format!("the token endpoint answered {status}"),
    }
}

/// The RSA private key the runner signs its client assertions with.
pub struct SigningKey(RsaKeyPair);

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(..)")
    }
}

impl SigningKey {
    /// Reads the key at `path`, refusing a file that others can read.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError::Read`] when the file cannot be read,
    /// [`KeyError::NotAFile`] when it is not a regular file,
    /// [`KeyError::Shared`] when its group or others can read it and
    /// [`KeyError::NotRsa`] when it holds no RSA private key in PEM.
    pub fn load(path: &Path) -> Result<Self, KeyError> {
        let shown = path.display().to_string();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(KeyError::Read {
                    path: shown,
                    message: error.to_string(),
                });
            }
        };
        if !metadata.is_file() {
            return Err(KeyError::NotAFile { path: shown });
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Err(KeyError::Shared { path: shown, mode });
        }
        let pem = match std::fs::read(path) {
            Ok(pem) => pem,
            Err(error) => {
                return Err(KeyError::Read {
                    path: shown,
                    message: error.to_string(),
                });
            }
        };
        match SigningKey::from_pem(&pem) {
            Some(key) => Ok(key),
            None => Err(KeyError::NotRsa { path: shown }),
        }
    }

    /// Parses an RSA private key in PKCS#8 or PKCS#1 PEM.
    ///
    /// Returns [`None`] when `pem` holds no such key.
    #[must_use]
    pub fn from_pem(pem: &[u8]) -> Option<Self> {
        let Ok(der) = PrivateKeyDer::from_pem_slice(pem) else {
            return None;
        };
        let parsed = match &der {
            PrivateKeyDer::Pkcs8(der) => RsaKeyPair::from_pkcs8(der.secret_pkcs8_der()),
            PrivateKeyDer::Pkcs1(der) => RsaKeyPair::from_der(der.secret_pkcs1_der()),
            PrivateKeyDer::Sec1(_) => return None,
            _ => return None,
        };
        match parsed {
            Ok(pair) => Some(SigningKey(pair)),
            Err(_rejected) => None,
        }
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, AuthError> {
        let SigningKey(pair) = self;
        let mut signature = vec![0; pair.public_modulus_len()];
        match pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message,
            &mut signature,
        ) {
            Ok(()) => Ok(signature),
            Err(_unspecified) => Err(AuthError::Assertion),
        }
    }
}

/// A bearer token for the farm.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    /// Returns the token as the `Authorization` header carries it.
    #[must_use]
    pub fn secret(&self) -> &str {
        let AccessToken(value) = self;
        value
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(..)")
    }
}

struct Cached {
    token: AccessToken,
    renew_at: Instant,
}

/// The source of the runner's access tokens.
pub struct TokenSource {
    http: reqwest::Client,
    credentials: Credentials,
    key: SigningKey,
    sleeper: Arc<dyn Sleeper>,
    cached: Mutex<Option<Cached>>,
}

#[derive(Serialize)]
struct AssertionHeader<'a> {
    alg: &'a str,
    typ: &'a str,
    kid: &'a str,
}

#[derive(Serialize)]
struct AssertionClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    jti: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenBody {
    access_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

impl TokenSource {
    /// Returns a source that asks for tokens as `credentials` with `key`.
    ///
    /// `sleeper` supplies the instant a cached token is measured against.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Transport`] when the HTTP client cannot be built.
    pub fn new(
        credentials: Credentials,
        key: SigningKey,
        sleeper: Arc<dyn Sleeper>,
    ) -> Result<Self, AuthError> {
        let built = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TOKEN_TIMEOUT)
            .build();
        let http = match built {
            Ok(http) => http,
            Err(_error) => {
                return Err(AuthError::Transport(
                    "the HTTP client could not be built".to_string(),
                ));
            }
        };
        Ok(TokenSource {
            http,
            credentials,
            key,
            sleeper,
            cached: Mutex::new(None),
        })
    }

    /// Returns the cached token, or a new one once the cached one is due.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Transport`] when the token endpoint cannot be
    /// reached, [`AuthError::Refused`] when it refuses the request,
    /// [`AuthError::Unusable`] when its answer holds no token and
    /// [`AuthError::Assertion`] when the assertion cannot be signed.
    pub async fn token(&self) -> Result<AccessToken, AuthError> {
        let mut cached = self.cached.lock().await;
        if let Some(Cached { token, renew_at }) = cached.as_ref()
            && self.sleeper.now() < *renew_at
        {
            return Ok(token.clone());
        }
        let requested = self.sleeper.now();
        let (token, lifetime) = self.request().await?;
        *cached = Some(Cached {
            token: token.clone(),
            renew_at: requested + lifetime.saturating_sub(TOKEN_RENEW_MARGIN),
        });
        Ok(token)
    }

    /// Drops `rejected` from the cache, unless a newer token already replaced it.
    pub async fn reject(&self, rejected: &AccessToken) {
        let mut cached = self.cached.lock().await;
        let Some(Cached { token, renew_at: _ }) = cached.as_ref() else {
            return;
        };
        if token == rejected {
            *cached = None;
        }
    }

    async fn request(&self) -> Result<(AccessToken, Duration), AuthError> {
        let Credentials {
            issuer: _,
            token_endpoint,
            client_id,
            key_id: _,
            resource,
        } = &self.credentials;
        let assertion = self.assertion(unix_now(), &jti()?)?;
        let form = [
            ("grant_type", GRANT_TYPE),
            ("client_id", client_id.as_str()),
            ("client_assertion_type", ASSERTION_TYPE),
            ("client_assertion", assertion.as_str()),
            ("resource", resource.as_str()),
            ("scope", SCOPE),
        ];
        let sent = self.http.post(token_endpoint).form(&form).send().await;
        let response = match sent {
            Ok(response) => response,
            Err(error) => return Err(transport(&error)),
        };
        let status = response.status().as_u16();
        let body = match read_body(response).await {
            Ok(body) => body,
            Err(error) => return Err(transport(&error)),
        };
        answer(status, &body)
    }

    fn assertion(&self, now: u64, jti: &str) -> Result<String, AuthError> {
        let Credentials {
            issuer,
            token_endpoint: _,
            client_id,
            key_id,
            resource: _,
        } = &self.credentials;
        let header = AssertionHeader {
            alg: "RS256",
            typ: "JWT",
            kid: key_id,
        };
        let claims = AssertionClaims {
            iss: client_id.as_str(),
            sub: client_id.as_str(),
            aud: issuer,
            jti,
            iat: now,
            exp: now + ASSERTION_LIFETIME.as_secs(),
        };
        let (Ok(header), Ok(claims)) = (serde_json::to_vec(&header), serde_json::to_vec(&claims))
        else {
            return Err(AuthError::Assertion);
        };
        let mut signed = URL_SAFE_NO_PAD.encode(header);
        signed.push('.');
        signed.push_str(&URL_SAFE_NO_PAD.encode(claims));
        let signature = self.key.sign(signed.as_bytes())?;
        signed.push('.');
        signed.push_str(&URL_SAFE_NO_PAD.encode(signature));
        Ok(signed)
    }
}

async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut response = response;
    let mut collected = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = MAX_TOKEN_BODY_BYTES - collected.len();
        if chunk.len() >= room {
            collected.extend_from_slice(&chunk[..room]);
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

fn answer(status: u16, body: &[u8]) -> Result<(AccessToken, Duration), AuthError> {
    if !(200..300).contains(&status) {
        let code = match serde_json::from_slice::<ErrorBody>(body) {
            Ok(ErrorBody { error }) => error,
            Err(_undecodable) => None,
        };
        return Err(AuthError::Refused { status, code });
    }
    let Ok(TokenBody {
        access_token,
        expires_in,
    }) = serde_json::from_slice::<TokenBody>(body)
    else {
        return Err(AuthError::Unusable);
    };
    let (Some(access_token), Some(expires_in)) = (access_token, expires_in) else {
        return Err(AuthError::Unusable);
    };
    if access_token.is_empty() || expires_in == 0 {
        return Err(AuthError::Unusable);
    }
    Ok((AccessToken(access_token), Duration::from_secs(expires_in)))
}

fn transport(error: &reqwest::Error) -> AuthError {
    if error.is_timeout() {
        return AuthError::Transport("timed out".to_string());
    }
    if error.is_connect() {
        return AuthError::Transport("connection failed".to_string());
    }
    AuthError::Transport("request failed".to_string())
}

fn unix_now() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs(),
        Err(_before_epoch) => 0,
    }
}

fn jti() -> Result<String, AuthError> {
    let mut bytes = [0u8; JTI_BYTES];
    if aws_lc_rs::rand::fill(&mut bytes).is_err() {
        return Err(AuthError::Assertion);
    }
    let mut encoded = String::with_capacity(JTI_BYTES * 2);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    Ok(encoded)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use base64::engine::general_purpose::STANDARD;

    use crate::protocol::client::TokioSleeper;

    fn generated() -> KeyPair {
        KeyPair::generate(KeySize::Rsa2048).unwrap()
    }

    fn pem(label: &str, der: &[u8]) -> String {
        let encoded = STANDARD.encode(der);
        let mut text = format!("-----BEGIN {label}-----\n");
        let mut line = String::new();
        for character in encoded.chars() {
            line.push(character);
            if line.len() == 64 {
                text.push_str(&line);
                text.push('\n');
                line.clear();
            }
        }
        if !line.is_empty() {
            text.push_str(&line);
            text.push('\n');
        }
        text.push_str(&format!("-----END {label}-----\n"));
        text
    }

    fn pkcs8(pair: &KeyPair) -> Vec<u8> {
        pair.as_der().unwrap().as_ref().to_vec()
    }

    fn element(der: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = der[0];
        let first = der[1];
        let (length, header) = if first < 0x80 {
            (usize::from(first), 2)
        } else {
            let count = usize::from(first & 0x7f);
            let mut length = 0;
            for byte in &der[2..2 + count] {
                length = length * 256 + usize::from(*byte);
            }
            (length, 2 + count)
        };
        (tag, &der[header..header + length], &der[header + length..])
    }

    fn pkcs1(pair: &KeyPair) -> Vec<u8> {
        let der = pkcs8(pair);
        let (_sequence, body, _after) = element(&der);
        let (_version, _value, rest) = element(body);
        let (_algorithm, _value, rest) = element(rest);
        let (tag, key, _rest) = element(rest);
        assert_eq!(tag, 0x04, "a PKCS#8 key ends in an octet string");
        key.to_vec()
    }

    pub(crate) fn credentials(issuer: &str) -> Credentials {
        Credentials {
            issuer: issuer.to_string(),
            token_endpoint: format!("{issuer}/api/oidc/token"),
            client_id: RunnerName("farm-runner-native".to_string()),
            key_id: "runner-key-1".to_string(),
            resource: "https://ralphex-farm.example.com/api".to_string(),
        }
    }

    pub(crate) fn token_source(issuer: &str) -> TokenSource {
        let key = SigningKey::from_pem(pem("PRIVATE KEY", &pkcs8(&generated())).as_bytes());
        TokenSource::new(credentials(issuer), key.unwrap(), Arc::new(TokioSleeper)).unwrap()
    }

    fn decoded(part: &str) -> serde_json::Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn a_pkcs8_and_a_pkcs1_key_are_both_read() {
        let pair = generated();
        assert!(SigningKey::from_pem(pem("PRIVATE KEY", &pkcs8(&pair)).as_bytes()).is_some());
        assert!(SigningKey::from_pem(pem("RSA PRIVATE KEY", &pkcs1(&pair)).as_bytes()).is_some());
    }

    #[test]
    fn text_that_is_no_rsa_key_is_refused() {
        assert!(SigningKey::from_pem(b"not a key").is_none());
        assert!(SigningKey::from_pem(pem("PRIVATE KEY", b"garbage").as_bytes()).is_none());
        let ec = aws_lc_rs::signature::EcdsaKeyPair::generate(
            &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        )
        .unwrap();
        let ec = ec.to_pkcs8v1().unwrap();
        assert!(SigningKey::from_pem(pem("PRIVATE KEY", ec.as_ref()).as_bytes()).is_none());
    }

    #[test]
    fn a_key_only_its_owner_can_read_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner.pem");
        std::fs::write(&path, pem("PRIVATE KEY", &pkcs8(&generated()))).unwrap();
        for mode in [0o600, 0o400] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(SigningKey::load(&path).is_ok(), "{mode:o}");
        }
    }

    #[test]
    fn a_key_others_can_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner.pem");
        std::fs::write(&path, pem("PRIVATE KEY", &pkcs8(&generated()))).unwrap();
        for mode in [0o640, 0o604, 0o644, 0o610] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let refused = SigningKey::load(&path).unwrap_err();
            assert_eq!(
                refused,
                KeyError::Shared {
                    path: path.display().to_string(),
                    mode,
                }
            );
            assert!(
                refused.to_string().contains(&format!("{mode:04o}")),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_missing_key_a_directory_and_a_file_without_a_key_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.pem");
        let Err(KeyError::Read { path, message: _ }) = SigningKey::load(&absent) else {
            panic!("a missing key is a read error");
        };
        assert_eq!(path, absent.display().to_string());

        let Err(KeyError::NotAFile { path: _ }) = SigningKey::load(dir.path()) else {
            panic!("a directory is not a key file");
        };

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o600)).unwrap();
        let Err(KeyError::NotRsa { path: _ }) = SigningKey::load(&empty) else {
            panic!("an empty file holds no key");
        };
    }

    #[test]
    fn the_assertion_is_an_rs256_jwt_with_the_contract_claims() {
        let pair = generated();
        let public = pair.public_key().as_ref().to_vec();
        let key = SigningKey::from_pem(pem("PRIVATE KEY", &pkcs8(&pair)).as_bytes()).unwrap();
        let source = TokenSource::new(
            credentials("https://auth.example.com"),
            key,
            Arc::new(TokioSleeper),
        )
        .unwrap();

        let assertion = source.assertion(1_800_000_000, "abc").unwrap();

        let parts: Vec<&str> = assertion.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decoded(parts[0]),
            serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "runner-key-1"})
        );
        assert_eq!(
            decoded(parts[1]),
            serde_json::json!({
                "iss": "farm-runner-native",
                "sub": "farm-runner-native",
                "aud": "https://auth.example.com",
                "jti": "abc",
                "iat": 1_800_000_000_u64,
                "exp": 1_800_000_060_u64,
            })
        );
        let signed = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
            .verify(signed.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn every_jti_is_fresh() {
        let first = jti().unwrap();
        let second = jti().unwrap();
        assert_eq!(first.len(), JTI_BYTES * 2);
        assert_ne!(first, second);
    }

    #[test]
    fn a_token_answer_yields_the_token_and_its_lifetime() {
        let (token, lifetime) = answer(
            200,
            br#"{"access_token":"tok","token_type":"bearer","expires_in":3600,"scope":"farm:run"}"#,
        )
        .unwrap();
        assert_eq!(token.secret(), "tok");
        assert_eq!(lifetime, Duration::from_secs(3600));
    }

    #[test]
    fn an_answer_without_a_usable_token_is_refused() {
        for body in [
            &br#"{"expires_in":3600}"#[..],
            br#"{"access_token":"tok"}"#,
            br#"{"access_token":"","expires_in":3600}"#,
            br#"{"access_token":"tok","expires_in":0}"#,
            br#"{"access_token":"tok","expires_in":-5}"#,
            b"not json",
        ] {
            assert_eq!(answer(200, body), Err(AuthError::Unusable));
        }
    }

    #[test]
    fn a_refusal_names_the_status_and_the_oauth_code_only() {
        let refused = answer(
            401,
            br#"{"error":"invalid_client","error_description":"the assertion carried key material"}"#,
        )
        .unwrap_err();
        assert_eq!(
            refused,
            AuthError::Refused {
                status: 401,
                code: Some("invalid_client".to_string()),
            }
        );
        assert_eq!(
            refused.to_string(),
            "the token endpoint answered 401 (invalid_client)"
        );
        let refused = answer(502, b"<html>bad gateway</html>").unwrap_err();
        assert_eq!(refused.to_string(), "the token endpoint answered 502");
    }

    #[test]
    fn neither_a_key_nor_a_token_prints_its_secret() {
        let pair = generated();
        let key = SigningKey::from_pem(pem("PRIVATE KEY", &pkcs8(&pair)).as_bytes()).unwrap();
        assert_eq!(format!("{key:?}"), "SigningKey(..)");
        let token = AccessToken("tok-secret".to_string());
        assert_eq!(format!("{token:?}"), "AccessToken(..)");
    }
}
