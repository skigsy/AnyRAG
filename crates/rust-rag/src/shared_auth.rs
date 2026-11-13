use super::{ApiError, ApiResult, bounded_file, unavailable};
pub(super) use crate::model::strict_json;
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    net::IpAddr,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use ureq::{Agent, http::Uri};

const MAX_JWKS: usize = 256 * 1024;
const KEY_LIFETIME: Duration = Duration::from_secs(60);
const UNKNOWN_KEY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_REFRESH_BACKOFF_SECONDS: u64 = 30;

pub(super) fn subject(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

pub(super) fn epoch() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn invalid_identity() -> ApiError {
    ApiError::new(
        401,
        "IDENTITY_REQUIRED",
        "Provide a valid API access token.",
    )
}

fn identity_unavailable() -> ApiError {
    ApiError::new(
        503,
        "IDENTITY_UNAVAILABLE",
        "The verification keys are unavailable.",
    )
    .retry_after(1)
}

#[derive(Debug)]
pub(super) struct Principal {
    pub subject: String,
    expires: u64,
}

impl Principal {
    pub fn ensure_current(&self) -> ApiResult<()> {
        if epoch().map_err(unavailable)? >= self.expires {
            return Err(invalid_identity());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub issuer: String,
    pub audience: String,
    pub jwks_url: Option<String>,
    pub jwks_file: Option<PathBuf>,
}

enum JwksSource {
    File(PathBuf),
    Http(Uri, Agent),
}

#[derive(Default)]
struct Keys {
    keys: BTreeMap<String, RsaPublicKeyComponents<Vec<u8>>>,
    loaded: Option<Instant>,
    refreshing: bool,
    unknown_not_before: Option<Instant>,
    retry_not_before: Option<Instant>,
    failures: u32,
}

impl Keys {
    fn fresh_at(&self, now: Instant) -> bool {
        self.loaded
            .is_some_and(|loaded| now.duration_since(loaded) < KEY_LIFETIME)
    }
}

pub(super) struct Authenticator {
    pub issuer: String,
    audience: String,
    source: JwksSource,
    keys: Mutex<Keys>,
}

impl Authenticator {
    pub fn new(config: Identity) -> Result<Self> {
        ensure!(
            !config.issuer.trim().is_empty()
                && config.issuer.len() <= 2048
                && subject(&config.audience),
            "Specify the exact issuer and API audience."
        );
        let source = match (config.jwks_file, config.jwks_url) {
            (Some(path), None) => {
                ensure!(path.is_absolute(), "Use an absolute public JWKS file path.");
                JwksSource::File(path)
            }
            (None, Some(url)) => {
                ensure!(
                    url.len() <= 2048 && !url.contains(['?', '#']),
                    "Use a fixed JWKS URL."
                );
                let uri = url.parse::<Uri>()?;
                let host = uri.host().unwrap_or_default();
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback());
                ensure!(
                    !host.is_empty()
                        && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
                        && (uri.scheme_str() == Some("https")
                            || loopback && uri.scheme_str() == Some("http")),
                    "Use HTTPS for the trusted JWKS URL."
                );
                let agent = Agent::config_builder()
                    .proxy(None)
                    .max_redirects(0)
                    .timeout_global(Some(Duration::from_secs(5)))
                    .max_response_header_size(32 * 1024)
                    .build()
                    .into();
                JwksSource::Http(uri, agent)
            }
            _ => anyhow::bail!("Specify exactly one trusted JWKS source."),
        };
        Ok(Self {
            issuer: config.issuer,
            audience: config.audience,
            source,
            keys: Mutex::default(),
        })
    }

    fn load(&self) -> Result<BTreeMap<String, RsaPublicKeyComponents<Vec<u8>>>> {
        let raw = match &self.source {
            JwksSource::File(path) => bounded_file(path, MAX_JWKS)?,
            JwksSource::Http(uri, agent) => {
                let mut response = agent.get(uri).header("Accept", "application/json").call()?;
                ensure!(
                    response.status().as_u16() == 200,
                    "Unexpected JWKS response."
                );
                let mut bytes = Vec::new();
                response
                    .body_mut()
                    .as_reader()
                    .take(MAX_JWKS as u64 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= MAX_JWKS, "JWKS exceeds its size limit.");
                bytes
            }
        };
        let value = strict_json(&raw)?;
        let items = value["keys"]
            .as_array()
            .filter(|items| items.len() <= 64)
            .ok_or_else(|| anyhow::anyhow!("Invalid JWKS."))?;
        let mut keys = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for item in items {
            let Some(fields) = item.as_object() else {
                continue;
            };
            let Some(kid) = item["kid"].as_str().filter(|kid| subject(kid)) else {
                continue;
            };
            ensure!(seen.insert(kid), "Duplicate JWKS key ID.");
            if item["kty"] != "RSA"
                || fields.get("alg").is_some_and(|v| v != "RS256")
                || fields.get("use").is_some_and(|v| v != "sig")
                || fields
                    .get("key_ops")
                    .is_some_and(|v| v != &serde_json::json!(["verify"]))
                || ["d", "p", "q", "dp", "dq", "qi", "oth"]
                    .iter()
                    .any(|name| fields.contains_key(*name))
            {
                continue;
            }
            let n = URL_SAFE_NO_PAD.decode(item["n"].as_str().unwrap_or_default())?;
            let e = URL_SAFE_NO_PAD.decode(item["e"].as_str().unwrap_or_default())?;
            ensure!(
                (256..=1024).contains(&n.len())
                    && n[0] != 0
                    && (n.len() > 256 || n[0] >= 128)
                    && (1..=5).contains(&e.len())
                    && e[0] != 0
                    && e[e.len() - 1] % 2 == 1
                    && (e.len() > 1 || e[0] >= 3),
                "Invalid JWKS RSA key."
            );
            keys.insert(kid.to_owned(), RsaPublicKeyComponents { n, e });
        }
        ensure!(!keys.is_empty(), "No usable verification keys.");
        Ok(keys)
    }

    pub fn preflight(&self) -> Result<()> {
        let mut state = self
            .keys
            .lock()
            .map_err(|_| anyhow::anyhow!("Identity lock failed."))?;
        ensure!(
            !state.refreshing,
            "Verification key refresh is already in progress."
        );
        state.refreshing = true;
        drop(state);
        self.refresh().map_err(|error| anyhow::anyhow!(error.2))
    }

    fn refresh(&self) -> ApiResult<()> {
        // The caller marked the single refresh in progress under the lock.
        // Transport stalls must not block users whose cached key is still fresh.
        let loaded = self.load();
        let mut state = self.keys.lock().map_err(|_| identity_unavailable())?;
        let now = Instant::now();
        state.refreshing = false;
        state.unknown_not_before = Some(now + UNKNOWN_KEY_INTERVAL);
        match loaded {
            Ok(keys) => {
                state.keys = keys;
                state.loaded = Some(now);
                state.failures = 0;
                state.retry_not_before = None;
                Ok(())
            }
            Err(_) => {
                let delay = (1_u64 << state.failures.min(5)).min(MAX_REFRESH_BACKOFF_SECONDS);
                state.failures = state.failures.saturating_add(1);
                state.retry_not_before = Some(now + Duration::from_secs(delay));
                Err(identity_unavailable())
            }
        }
    }

    fn verify(&self, kid: &str, input: &str, signature: &[u8]) -> ApiResult<()> {
        let mut state = self.keys.lock().map_err(|_| identity_unavailable())?;
        let now = Instant::now();
        let fresh = state.fresh_at(now);
        if !fresh || !state.keys.contains_key(kid) {
            if state.refreshing || state.retry_not_before.is_some_and(|at| now < at) {
                return Err(identity_unavailable());
            }
            if fresh && state.unknown_not_before.is_some_and(|at| now < at) {
                return Err(invalid_identity());
            }
            state.refreshing = true;
            drop(state);
            self.refresh()?;
            state = self.keys.lock().map_err(|_| identity_unavailable())?;
        }
        if !state.fresh_at(Instant::now()) {
            return Err(identity_unavailable());
        }
        // Verification and publishing a replacement key set are serialized.
        state
            .keys
            .get(kid)
            .ok_or_else(invalid_identity)?
            .verify(&RSA_PKCS1_2048_8192_SHA256, input.as_bytes(), signature)
            .map_err(|_| invalid_identity())
    }

    #[cfg(test)]
    pub(super) fn allow_unknown_refresh_for_fixture(&self) {
        self.keys.lock().unwrap().unknown_not_before = None;
    }

    #[cfg(test)]
    pub(super) fn expire_keys_for_fixture(&self) {
        self.keys.lock().unwrap().loaded = None;
    }

    pub fn authenticate(&self, header: Option<&str>) -> ApiResult<Principal> {
        let invalid = invalid_identity;
        let (scheme, token) = header
            .filter(|value| value.len() <= 8192)
            .and_then(|value| value.split_once(' '))
            .ok_or_else(invalid)?;
        if !scheme.eq_ignore_ascii_case("Bearer") {
            return Err(invalid());
        }
        let (input, signature) = token.rsplit_once('.').ok_or_else(invalid)?;
        let (metadata, claims) = input.split_once('.').ok_or_else(invalid)?;
        let unpack = |encoded| {
            let data = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?;
            strict_json(&data).map_err(|_| invalid())
        };
        let metadata = unpack(metadata)?;
        let fields = metadata.as_object().ok_or_else(invalid)?;
        let kid = metadata["kid"]
            .as_str()
            .filter(|kid| subject(kid))
            .ok_or_else(invalid)?;
        if metadata["alg"] != "RS256"
            || ["jku", "jwk", "x5u", "crit", "b64"]
                .iter()
                .any(|name| fields.contains_key(*name))
        {
            return Err(invalid());
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
        let claims = unpack(claims)?;
        let audience = &claims["aud"];
        let audience_matches = audience.as_str() == Some(self.audience.as_str())
            || audience.as_array().is_some_and(|audiences| {
                !audiences.is_empty()
                    && audiences.iter().all(Value::is_string)
                    && audiences
                        .iter()
                        .any(|v| v.as_str() == Some(self.audience.as_str()))
            });
        let now = epoch().map_err(unavailable)?;
        let expires = claims["exp"]
            .as_u64()
            .filter(|time| *time > now)
            .ok_or_else(invalid)?;
        let sub = claims["sub"]
            .as_str()
            .filter(|sub| subject(sub))
            .ok_or_else(invalid)?;
        if claims["iss"] != self.issuer
            || !audience_matches
            || claims
                .get("nbf")
                .is_some_and(|v| v.as_u64().is_none_or(|time| time > now))
        {
            return Err(invalid());
        }
        self.verify(kid, input, &signature)?;
        if epoch().map_err(unavailable)? >= expires {
            return Err(invalid());
        }
        Ok(Principal {
            subject: sub.to_owned(),
            expires,
        })
    }
}
