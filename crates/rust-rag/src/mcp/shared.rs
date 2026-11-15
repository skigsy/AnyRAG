use super::{Call, Citation, INVALID_PARAMS, params, tool_result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs::File, io::Read, net::IpAddr, path::Path, time::Duration};
use ureq::{
    Agent,
    http::{HeaderValue, Uri},
};

const GUIDED: &str = "/v1/relationship/guided";
const MAX_TOKEN: u64 = 16 * 1024;
const MAX_BODY: u64 = 256 * 1024;
const REQUEST_FAILED: &str = "Could not complete the request to the configured shared service.";
const EVIDENCE_UNAVAILABLE: &str = "This citation is unavailable at the requested revision.";

pub(super) struct Client {
    origin: Uri,
    authorization: HeaderValue,
    agent: Agent,
}

impl Client {
    pub(super) fn new(origin: &str, token_file: &Path) -> Result<Self, &'static str> {
        let origin = origin_uri(origin)?;
        let authorization = authorization(token_file)?;
        let agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(12)))
            .max_response_header_size(32 * 1024)
            .build()
            .into();
        Ok(Self {
            origin,
            authorization,
            agent,
        })
    }

    fn uri(&self, path: &str) -> Result<Uri, &'static str> {
        let mut parts = self.origin.clone().into_parts();
        parts.path_and_query = Some(path.parse().map_err(|_| REQUEST_FAILED)?);
        Uri::from_parts(parts).map_err(|_| REQUEST_FAILED)
    }

    fn guided(&self, request: &Guided<'_>) -> Result<Value, &'static str> {
        let bytes = serde_json::to_vec(request).map_err(|_| REQUEST_FAILED)?;
        let response = self
            .agent
            .post(self.uri(GUIDED)?)
            .header("Authorization", self.authorization.clone())
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send(bytes.as_slice())
            .map_err(|_| REQUEST_FAILED)?;
        self.response(response)
    }

    fn evidence(&self, citation: Citation) -> Result<Value, &'static str> {
        let url = self.uri(&format!("/v1/evidence/{}", citation.chunk_id))?;
        let response = self
            .agent
            .get(url)
            .header("Authorization", self.authorization.clone())
            .header("Accept", "application/json")
            .call()
            .map_err(|_| REQUEST_FAILED)?;
        let body = self.response(response)?;
        if body["evidence"]["chunk_id"] != citation.chunk_id
            || body["evidence"]["revision"] != citation.revision
        {
            return Err(EVIDENCE_UNAVAILABLE);
        }
        Ok(body)
    }

    fn response(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
    ) -> Result<Value, &'static str> {
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err("The shared service denied this signed caller."),
            404 => return Err(EVIDENCE_UNAVAILABLE),
            409 => {
                return Err(
                    "The shared relationship changed. Request a new preview before confirming.",
                );
            }
            400 => return Err("The shared service rejected the request parameters."),
            _ => return Err(REQUEST_FAILED),
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| REQUEST_FAILED)?;
        if bytes.len() as u64 > MAX_BODY {
            return Err("The shared response exceeds the MCP size limit.");
        }
        let body = crate::model::strict_json(&bytes).map_err(|_| REQUEST_FAILED)?;
        let secret = self
            .authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(REQUEST_FAILED)?;
        if !body.is_object() || body.get("error").is_some() || contains_secret(&body, secret) {
            return Err(REQUEST_FAILED);
        }
        Ok(body)
    }
}

fn origin_uri(origin: &str) -> Result<Uri, &'static str> {
    const INVALID: &str =
        "Configure a shared HTTPS origin or use HTTP with a numeric loopback address.";
    if origin.len() > 2048 || origin.contains(['?', '#']) {
        return Err(INVALID);
    }
    let uri: Uri = origin.parse().map_err(|_| INVALID)?;
    let host = uri.host().ok_or(INVALID)?;
    if host.is_empty()
        || uri
            .authority()
            .is_none_or(|value| value.as_str().contains('@'))
        || uri.path() != "/"
    {
        return Err(INVALID);
    }
    let loopback = host
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    if uri.scheme_str() != Some("https") && !(uri.scheme_str() == Some("http") && loopback) {
        return Err(INVALID);
    }
    Ok(uri)
}

fn authorization(path: &Path) -> Result<HeaderValue, &'static str> {
    const INVALID: &str = "Use a private caller bearer-token file containing a single token.";
    if !path.is_absolute() || !path.is_file() {
        return Err(INVALID);
    }
    let file = File::open(path).map_err(|_| INVALID)?;
    let metadata = file.metadata().map_err(|_| INVALID)?;
    if !metadata.is_file() {
        return Err(INVALID);
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(INVALID);
    }
    let mut token = String::new();
    file.take(MAX_TOKEN + 1)
        .read_to_string(&mut token)
        .map_err(|_| INVALID)?;
    if token.len() as u64 > MAX_TOKEN {
        return Err(INVALID);
    }
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty()
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=".contains(&byte))
    {
        return Err(INVALID);
    }
    let mut header = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| INVALID)?;
    header.set_sensitive(true);
    Ok(header)
}

fn contains_secret(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(items) => items.iter().any(|item| contains_secret(item, secret)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(secret) || contains_secret(value, secret)),
        _ => false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    request: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    request: Box<RawValue>,
    confirmed: Box<RawValue>,
}

#[derive(Serialize)]
struct Guided<'a> {
    request: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed: Option<&'a RawValue>,
}

fn is_object(value: &RawValue) -> bool {
    value.get().trim_start().starts_with('{')
}

pub(super) fn tools() -> Value {
    let request = json!({"type":"object", "description":"Typed relationship request from the signed service contract."});
    let annotations = json!({"readOnlyHint":true, "destructiveHint":false, "idempotentHint":true, "openWorldHint":true});
    json!({"tools":[
        {"name":"preview_relationship", "description":"Preview a typed relationship as the configured signed reader. The service withholds the terminal value.",
         "inputSchema":{"type":"object", "properties":{"request":request}, "required":["request"], "additionalProperties":false},
         "annotations":annotations},
        {"name":"confirm_relationship", "description":"Confirm the exact interpretation returned by preview. The service verifies reader access and current evidence before returning citations.",
         "inputSchema":{"type":"object", "properties":{"request":request, "confirmed":{"type":"object", "description":"The exact interpretation returned by preview."}},
                        "required":["request", "confirmed"], "additionalProperties":false}, "annotations":annotations},
        {"name":"open_citation", "description":"Reopen an exact shared evidence reference as the configured signed reader.",
         "inputSchema":{"type":"object", "properties":{"chunk_id":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"},
                                                            "revision":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"}},
                        "required":["chunk_id", "revision"], "additionalProperties":false}, "annotations":annotations}
    ]})
}

pub(super) fn call(client: &Client, raw: Option<&RawValue>) -> Result<Value, (i32, &'static str)> {
    let Call {
        name, arguments, ..
    } = params(raw)?;
    let result = match name.as_str() {
        "preview_relationship" => {
            let Preview { request } = params(Some(&arguments))?;
            if !is_object(&request) {
                return Err(INVALID_PARAMS);
            }
            client.guided(&Guided {
                request: &request,
                confirmed: None,
            })
        }
        "confirm_relationship" => {
            let Confirm { request, confirmed } = params(Some(&arguments))?;
            if !is_object(&request) || !is_object(&confirmed) {
                return Err(INVALID_PARAMS);
            }
            client.guided(&Guided {
                request: &request,
                confirmed: Some(&confirmed),
            })
        }
        "open_citation" => {
            let citation: Citation = params(Some(&arguments))?;
            if !citation.valid() {
                return Err(INVALID_PARAMS);
            }
            client.evidence(citation)
        }
        _ => return Err(INVALID_PARAMS),
    };
    Ok(tool_result(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{Backend, run};
    use std::{env, fs, process::Command, thread};
    use tiny_http::{Header, Method, Response, Server, StatusCode};

    #[test]
    fn shared_stdio_uses_one_signed_reader_and_only_the_guided_contract() {
        const PROXY_TEST: &str = "ANYRAG_MCP_TEST_WITH_ENV_PROXY";
        if env::var_os(PROXY_TEST).is_none() {
            // Keep the proxy environment private to the child test process.
            let trap = Server::http("127.0.0.1:0").unwrap();
            let proxy = format!("http://{}", trap.server_addr());
            let mut child = Command::new(env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "mcp::shared::tests::shared_stdio_uses_one_signed_reader_and_only_the_guided_contract",
                    "--nocapture",
                ])
                .env(PROXY_TEST, "1")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy");
            for setting in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                child.env(setting, &proxy);
            }
            let completed = child.output().unwrap();
            assert!(
                completed.status.success(),
                "{}{}",
                String::from_utf8_lossy(&completed.stdout),
                String::from_utf8_lossy(&completed.stderr)
            );
            assert!(
                trap.recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
            return;
        }
        assert!(Agent::config_builder().build().proxy().is_some());
        for blocked in [
            "http://search.example",
            "http://localhost:9999",
            "https://user@search.example",
            "https://search.example/v1/query",
            "https://search.example/?token=abc",
        ] {
            assert!(origin_uri(blocked).is_err());
        }
        assert!(origin_uri("http://[::1]:9999").is_ok());
        assert!(origin_uri("https://search.example").is_ok());
        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("reader.jwt");
        const TOKEN: &str = "synthetic.signed.reader";
        fs::write(&token_path, format!("{TOKEN}\n")).unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o640)).unwrap();
            assert!(authorization(&token_path).is_err());
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let server = Server::http("127.0.0.1:0").unwrap();
        let client = Client::new(&format!("http://{}", server.server_addr()), &token_path).unwrap();
        assert!(client.agent.config().proxy().is_none());
        fs::write(&token_path, "synthetic.rotated.reader\n").unwrap();
        let typed = json!({"root":"Arrival Desk", "path_predicates":["uses"], "scope":["day"],
                          "lookup_context":{"observed_name":"welcome counter"}});
        let interpretation = json!({"root":"Arrival Desk", "path_predicates":["uses"], "scope":["day"],
                                    "identity":{"status":"reviewed_alias"}});
        let chunk = "a".repeat(64);
        let revision = "b".repeat(64);
        let stale = "c".repeat(64);
        let preview = json!({"status":"confirm_relationship", "interpretation":interpretation});
        let confirmed = json!({"status":"value", "value":"Current Kit", "name_bindings":[{"chunk_id":chunk,"revision":revision}]});
        let evidence =
            json!({"evidence":{"chunk_id":chunk,"revision":revision,"text":"Current quote."}});
        let worker = thread::spawn({
            let typed = typed.clone();
            let interpretation = interpretation.clone();
            let chunk = chunk.clone();
            let preview = preview.clone();
            let confirmed = confirmed.clone();
            let evidence = evidence.clone();
            move || {
                let signed = format!("Bearer {TOKEN}");
                let citation = format!("/v1/evidence/{chunk}");
                for (method, path, body, status, result) in [
                    (Method::Post, GUIDED, json!({"request":typed}), 200, preview),
                    (
                        Method::Post,
                        GUIDED,
                        json!({"request":typed,"confirmed":interpretation}),
                        200,
                        confirmed,
                    ),
                    (
                        Method::Get,
                        citation.as_str(),
                        Value::Null,
                        200,
                        evidence.clone(),
                    ),
                    (Method::Get, citation.as_str(), Value::Null, 200, evidence),
                    (
                        Method::Post,
                        GUIDED,
                        json!({"request":typed}),
                        307,
                        json!({"error":TOKEN}),
                    ),
                    (
                        Method::Post,
                        GUIDED,
                        json!({"request":typed}),
                        200,
                        json!({"nested":[{"echo":TOKEN}]}),
                    ),
                    (
                        Method::Get,
                        citation.as_str(),
                        Value::Null,
                        403,
                        json!({"error":TOKEN}),
                    ),
                ] {
                    let mut incoming = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap_or_else(|| {
                            panic!("Missing {method:?} {path} (fixture HTTP {status})")
                        });
                    assert_eq!(incoming.method(), &method);
                    assert_eq!(incoming.url(), path);
                    assert!(
                        incoming
                            .headers()
                            .iter()
                            .any(|h| h.field.equiv("Authorization") && h.value.as_str() == signed)
                    );
                    if method == Method::Post {
                        let parsed: Value = serde_json::from_reader(incoming.as_reader()).unwrap();
                        assert_eq!(parsed, body);
                    }
                    let mut response = Response::from_string(result.to_string())
                        .with_status_code(StatusCode(status));
                    if status == 307 {
                        response
                            .add_header(Header::from_bytes("Location", "/v1/admin/sync").unwrap());
                    }
                    incoming.respond(response).unwrap();
                }
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(50))
                        .unwrap()
                        .is_none()
                );
            }
        });

        let mut input = Vec::new();
        let mut append = |message: Value| {
            serde_json::to_writer(&mut input, &message).unwrap();
            input.push(b'\n');
        };
        append(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
        );
        append(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        append(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        for (id, name, args) in [
            (3, "search", json!({"question":"private data"})),
            (
                4,
                "preview_relationship",
                json!({"request":typed,"token":"other"}),
            ),
            (5, "preview_relationship", json!({"request":typed})),
            (
                6,
                "confirm_relationship",
                json!({"request":typed,"confirmed":interpretation}),
            ),
            (
                7,
                "open_citation",
                json!({"chunk_id":chunk,"revision":revision}),
            ),
            (
                8,
                "open_citation",
                json!({"chunk_id":chunk,"revision":stale}),
            ),
            (9, "preview_relationship", json!({"request":typed})),
            (10, "preview_relationship", json!({"request":typed})),
            (
                11,
                "open_citation",
                json!({"chunk_id":chunk,"revision":revision}),
            ),
        ] {
            append(
                json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}),
            );
        }
        let mut stdout = Vec::new();
        run(&Backend::Shared(&client), input.as_slice(), &mut stdout).unwrap();
        worker.join().unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(!stdout.contains(TOKEN));
        let frames: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let instructions = frames[0]["result"]["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("untrusted data") && instructions.contains("hosted MCP client")
        );
        let names: Vec<_> = frames[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "preview_relationship",
                "confirm_relationship",
                "open_citation"
            ]
        );
        assert_eq!(frames[2]["error"]["code"], -32602);
        assert_eq!(frames[3]["error"]["code"], -32602);
        for (frame, expected) in [(4, preview), (5, confirmed), (6, evidence)] {
            assert_eq!(frames[frame]["result"]["structuredContent"], expected);
        }
        for result in &frames[7..] {
            assert_eq!(result["result"]["isError"], true);
        }
    }
}
