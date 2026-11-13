use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use std::{
    fs,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const ISSUER: &str = "https://issuer.example.invalid";
const PRIVATE_CANARY: &str = "SYNTHETIC_PRIVATE_XYZZY";
const PUBLIC_QUESTION: &str = "Lyra beacon";
// One-use synthetic RSA key generated for this fixture. Only its public
// components and signatures for issuer.example.invalid were retained.
const N: &str = "tya7wApV_b40QhCpUCYEMcfFD0WnBbKz-tnlciMJHhxy9EjZufmMnvHPTuTQ-vGHVmjwq7SmCopABJnjQpPME1aSfYY5i4oVmvGMLRyz0a1wjyRZMEHvYer3-21J6QmMwfIWcH1njSRu7vakDEZr8KceBQ6-pqmUf0aYjcjrsWFOugY22VNJvp405hwt4DHHTDNrgtCt0wFTHJxiHzyHijNNXv9X_5dxAXHwljSJ4j5rOuujzeZVTj5bdH7J96j15lXEQGuaWAonpkxf_um6jxpn4H2ya9M2-dHbzxBEf-KHYYSgbmsZ25h0l2bsTEzhpn68G-7eYmQ34CG9tx4fSQ";
const E: &str = "AQAB";
const SIGNED_HEADER: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3QiLCJ0eXAiOiJhdCtqd3QifQ";
fn signed_jwt(claims: &str, signature: &str) -> String {
    format!("{SIGNED_HEADER}.{claims}.{signature}")
}
static ADMIN: LazyLock<String> = LazyLock::new(|| {
    signed_jwt(
        "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLmludmFsaWQiLCJzdWIiOiJhZG1pbiIsImV4cCI6NDA3MDkwODgwMCwiYXVkIjoiYXBpIn0",
        "RqXTgIrBzeN8r75C1BIiKP55O_T_EHRa4KtiDLvoi90x4s7MAtWgqMT1sNSFFmfwPKy9xjOrfFkU4xdysCtllm8T3T_FfBMI9e9BDZp0vD30Fguce35eHQyI7uJwrfCiQiwkIf9G6EMgr_iColOqWPaOWez3vh0FGFxGznItBsNleucR0LC5SexVTdK6Q0CBELla1uzbQCTJP1H5LGQZgvBAV08BHQ6Nm_tNyc4A8wxme0OnBF_vUPUzgcmGWXyYtoyU4Y-8C7V3PWD7MlAxiXSVbq6E7uF_pk90FbxEJ92TWVLLAR1RkDbVrzJ4A-bqXLESLxrTpq6-cOnCRCNC7w",
    )
});
static ALICE: LazyLock<String> = LazyLock::new(|| {
    signed_jwt(
        "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLmludmFsaWQiLCJzdWIiOiJhbGljZSIsImV4cCI6NDA3MDkwODgwMCwiYXVkIjoiYXBpIn0",
        "YwBR0Yi0ZrxrZmEfnx9Jam9-Sxe5RMBU6JryKH6hRf5nNZ_mzIrV16LO-ulGJ6sOdX4-aKyPqZG0MYA0vf-JOM7jyeIKYiz7eWblL85vI7WaQXnr5iWDGzy0ZfW-1VyH4-Ak2yvvteJExukt7FaCrLLFZpeuote9BRaGpNssiull4e6SfhLPZf5Jz_t5UbCWDCPaP4uGc-dDCs74Ui2-uKZaOOFLP06kOUOmpyqZqBmfyCCBlcsqpS0gs1T_hvCiIKBD7Dasml8_Ts7e21iPPNZnoNY_kOxe_uR9EiMCX6RuplwsvXR02LEyEZXBXhmpTufYnul4cQd6vFQHXpQaRA",
    )
});
static BOB: LazyLock<String> = LazyLock::new(|| {
    signed_jwt(
        "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLmludmFsaWQiLCJzdWIiOiJib2IiLCJleHAiOjQwNzA5MDg4MDAsImF1ZCI6ImFwaSJ9",
        "Q3MgzS6qNfxSWPzVDkOunDrAcHnXGIZDPb2aWiDhvBSORTT13zjAxc57ECYm5nY91KNEGoh0zsRAe2HouBhSIdXgusGMgyPrSeSG9SnAIiOhginbybuT4CMdHh5hPYjwPtrUT5JyVylZWWhH-JuSbvJ7lGif76P9KB7v7jsFwdILacCS2YlkBMAU6EiB_fMjfzDm3jesJdPLmZScMMRSo_K0JxD4VTJLrbnsJn6YXqDTmWsq4naLiKCSJF93d8BDw-ZyUcn23ndr7gAmVD0erTJa-g052r7XRSSnWWcAFgq_NvNfeBj37uA_BkK1VP2BkPKLdGfNzY7stFP4c3zz3A",
    )
});
static WRONG_AUD: LazyLock<String> = LazyLock::new(|| {
    signed_jwt(
        "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLmludmFsaWQiLCJzdWIiOiJhbGljZSIsImV4cCI6NDA3MDkwODgwMCwiYXVkIjoiZGlmZmVyZW50LWFwaSJ9",
        "Q7aBBSNRNP8b9MsKcFs5R6QU-V6Nh-t7-vhwz37gxyIWltPJE5uBMaevcCqcfWKpON-2ThKspO2N_-O87HkXKDjwWScWGOJhVIhkg-eVZFCvlJ-x8aXWyZK2Nk1XZKYdTZhBrDnl9Ib_lDr1OUE18XWk8kBpcu4tXnoWJzFuHe1w_s6bSKNJ3KU2HY2tKz-wZwYXQ4OX0zRImy855Z99jeC7Q5UML3Ot9awYY3su4L66KHC6vJIJw7yjxMfW0K0KU-5Mpfjj8IzpESolUr-DAK4iIkyq6psX2gyxDDWLZLHbW9PbprOZsjHYk02VC2h_kJylyHPa1RTBXhxlxGGrKQ",
    )
});
static EXPIRED: LazyLock<String> = LazyLock::new(|| {
    signed_jwt(
        "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLmludmFsaWQiLCJzdWIiOiJhbGljZSIsImV4cCI6OTQ2Njg0ODAwLCJhdWQiOiJhcGkifQ",
        "e5WOCxyaX4m-Fg7dMwbCfzrOm6GwyZRpleXHyN_Kx5Dz-kBHpCQDkGuzoDgmrbF6_d7B1FEL8PvBpcwP5QKdj2jlRmnGlH35dvQv09hxEYtX4Wu47eFWX_R44QjIibG2WJHfPt5EkiDm4g_KRAT-OPgt0Zv83D0SFfGm8lXNjoIkIR6CFbUHTGNBxlUNYeM_Sf2w-aoY_QFLLDB7v2ez4HU5h7XKY0_HLZgIBl_2Xu739qqDSbLxqi9xicE4rsKrbExUadekzb59WaMHkCEitTolVXnK_G7B7WVAlE9YeC5ntd7CVWeVG7v600mIMmAhJmBUcbLAeuIthUoLq1iPZA",
    )
});

struct Fixture {
    root: tempfile::TempDir,
    profile: PathBuf,
}

impl Fixture {
    fn new(model: Option<&str>, external_embedding: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        for name in ["policy", "policy/versions", "backup", "open", "restricted"] {
            files::state_directory(&path.join(name), true).unwrap();
        }
        fs::write(path.join("open/seed.md"), "Lyra beacon links Relay Point.").unwrap();
        fs::write(
            path.join("open/hop.md"),
            "Relay Point connects Archive Vault.",
        )
        .unwrap();
        fs::write(
            path.join("restricted/secret.md"),
            format!("Lyra beacon and Relay Point identify {PRIVATE_CANARY}."),
        )
        .unwrap();
        let jwks = path.join("jwks.json");
        fs::write(
            &jwks,
            json!({"keys":[{"kty":"RSA","kid":"test","alg":"RS256","use":"sig","n":N,"e":E}]})
                .to_string(),
        )
        .unwrap();
        let mut profile = json!({
            "schema_version":1, "state_dir":path.join("state"),
            "identity":{"issuer":ISSUER,"audience":"api","jwks_file":jwks},
            "policy":{"kind":"file","directory":path.join("policy")},
            "backup":{"kind":"file","directory":path.join("backup")},
            "sources":{"open":{"kind":"folder","root":path.join("open")},
                "restricted":{"kind":"folder","root":path.join("restricted")}},
            "graph":{"mode":"typed"},
            "embedding":{"provider":"hash","model":"lexical-hash-v1","dimensions":64},
        });
        if let Some(model) = model {
            let config = path.join("answer.json");
            fs::write(
                &config,
                json!({"url":format!("http://{model}/v1/chat/completions"),
                "model":"synthetic","timeout_seconds":3})
                .to_string(),
            )
            .unwrap();
            profile["endpoints"] = json!({"chat":config});
            if external_embedding {
                let config = path.join("embedding.json");
                fs::write(
                    &config,
                    json!({"url":format!("http://{model}/v1/embeddings"),
                    "model":"synthetic","timeout_seconds":3,"dimensions":2})
                    .to_string(),
                )
                .unwrap();
                profile["embedding"] =
                    json!({"provider":"endpoint","name":"embedding","config":config});
            }
        }
        let profile_path = path.join("profile.json");
        fs::write(&profile_path, profile.to_string()).unwrap();
        let fixture = Self {
            root,
            profile: profile_path,
        };
        fixture.policy(1, true, model.is_some(), external_embedding);
        fixture
    }

    fn policy(&self, version: i64, alice: bool, chat: bool, embedding: bool) {
        self.policy_with_restricted_reader(version, alice, chat, embedding, "bob");
    }

    fn policy_with_restricted_reader(
        &self,
        version: i64,
        alice: bool,
        chat: bool,
        embedding: bool,
        restricted_reader: &str,
    ) {
        let open_endpoints: Vec<_> = [(chat, "chat"), (embedding, "embedding")]
            .into_iter()
            .filter_map(|(allowed, endpoint)| allowed.then_some(endpoint))
            .collect();
        let policy = json!({"schema_version":1,"version":version,"issuer":ISSUER,
            "administrators":["admin"],"subjects":{"alice":if alice { vec!["reader"] } else { vec![] }},
            "collections":{"open":{"roles":["reader"],"endpoints":open_endpoints},
                "restricted":{"subjects":[restricted_reader]}}});
        self.write_policy(policy);
    }

    fn write_policy(&self, policy: Value) {
        let version = policy["version"].as_i64().unwrap();
        let policy = policy.to_string();
        let path = self.root.path().join("policy");
        fs::write(
            path.join("versions").join(format!("{version}.json")),
            &policy,
        )
        .unwrap();
        let update = path.join("next.json");
        fs::write(&update, policy).unwrap();
        fs::rename(&update, path.join("current.json")).unwrap();
    }

    fn open(&self) -> Arc<Service> {
        Arc::new(Service::open(&self.profile).unwrap())
    }

    fn rewind(&self, version: i64) {
        let path = self.root.path().join("policy");
        fs::copy(
            path.join("versions").join(format!("{version}.json")),
            path.join("current.json"),
        )
        .unwrap();
    }
}

fn api(address: &str, token: Option<&str>, path: &str, body: Option<Value>) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let url = format!("http://{address}{path}");
    let auth = token.map(|token| format!("Bearer {token}"));
    let mut response = if let Some(body) = body {
        let mut request = agent.post(&url).header("Content-Type", "application/json");
        if let Some(header) = &auth {
            request = request.header("Authorization", header);
        }
        request.send(body.to_string()).unwrap()
    } else {
        let mut request = agent.get(&url);
        if let Some(header) = &auth {
            request = request.header("Authorization", header);
        }
        request.call().unwrap()
    };
    assert_eq!(response.headers()["Cache-Control"], "no-store");
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .map(|header| header.to_str().unwrap().parse::<u64>().unwrap());
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(body["error"]["retry_after_seconds"].as_u64(), retry_after);
    (status, body)
}

fn with_http<T>(service: &Arc<Service>, check: impl FnOnce(&str) -> T) -> T {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let _worker = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                if let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap() {
                    handle(service, request);
                }
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&address)));
        done.store(true, Ordering::Release);
        match result {
            Ok(result) => result,
            Err(error) => std::panic::resume_unwind(error),
        }
    })
}

fn query(address: &str, token: &str, question: &str, graph: bool) -> Value {
    let (status, body) = api(
        address,
        Some(token),
        "/v1/query",
        Some(json!({"question":question,"graph":graph})),
    );
    assert_eq!(status, 200, "{body}");
    body
}

fn sync_http(address: &str) -> Value {
    assert_eq!(
        api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/sync",
            Some(json!({}))
        )
        .0,
        202
    );
    await_sync(address)
}

fn await_sync(address: &str) -> Value {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, report) = api(address, Some(ADMIN.as_str()), "/v1/admin/status", None);
        assert_eq!(status, 200, "{report}");
        if report["sync"]["status"] != "running" {
            return report["sync"].clone();
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn low_limit_prefers_strong_authorized_evidence() {
    let fixture = Fixture::new(None, false);
    let question = "Lyra beacon capsule coolant schedule";
    fs::write(
        fixture.root.path().join("restricted/secret.md"),
        format!("{question}. {PRIVATE_CANARY}."),
    )
    .unwrap();
    fixture.policy_with_restricted_reader(2, true, false, false, "alice");
    let service = fixture.open();
    with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete");
        let one = || {
            let (status, result) = api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":question,"graph":false,"limit":1})),
            );
            assert_eq!(status, 200);
            result
        };
        let result = one();
        assert_eq!(result["status"], "evidence");
        assert_eq!(result["evidence"][0]["collection"], "restricted");
        assert!(result.to_string().contains(PRIVATE_CANARY));
        fixture.policy(3, true, false, false);
        let result = one();
        assert_eq!(result["status"], "weak_match");
        assert_eq!(result["evidence"][0]["collection"], "open");
        assert!(!result.to_string().contains(PRIVATE_CANARY));
    });
}

#[test]
fn jwks_stall_rotation_and_failure_are_bounded() {
    let issuer = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let auth = Authenticator::new(Identity {
        issuer: ISSUER.into(),
        audience: "api".into(),
        jwks_url: Some(format!("http://{}/jwks", issuer.server_addr())),
        jwks_file: None,
    })
    .unwrap();
    let jwks = |kid| json!({"keys":[{"kty":"RSA","kid":kid,"n":N,"e":E}]});
    let next_request = || {
        let request = issuer
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/jwks");
        request
    };
    let known = format!("Bearer {}", ALICE.as_str());
    let unknown = format!(
        "Bearer {}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","kid":"unknown"}"#),
        ALICE.split_once('.').unwrap().1
    );
    let status = |token: &str| {
        auth.authenticate(Some(token))
            .map_or_else(|error| error.0, |_| 200)
    };
    thread::scope(|scope| {
        let initial = scope.spawn(|| auth.preflight().unwrap());
        model_reply(next_request(), jwks("test"));
        initial.join().unwrap();
        assert_eq!(status(&known), 200);
        assert_eq!(status(&unknown), 401);
        assert!(issuer.try_recv().unwrap().is_none());

        auth.allow_unknown_refresh_for_fixture();
        let leader = scope.spawn(|| status(&unknown));
        let refresh = next_request();
        let start = Instant::now();
        let followers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    assert_eq!(status(&known), 200);
                    assert_eq!(status(&unknown), 503);
                })
            })
            .collect();
        for follower in followers {
            follower.join().unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(issuer.try_recv().unwrap().is_none());
        model_reply(refresh, jwks("rotated"));
        assert_eq!(leader.join().unwrap(), 401);
        assert_eq!(status(&known), 401);

        auth.allow_unknown_refresh_for_fixture();
        let restore_known = scope.spawn(|| status(&known));
        model_reply(next_request(), jwks("test"));
        assert_eq!(restore_known.join().unwrap(), 200);
        auth.expire_keys_for_fixture();
        let stale = scope.spawn(|| status(&known));
        let refresh = next_request();
        assert_eq!(status(&known), 503);
        refresh.respond(tiny_http::Response::empty(429)).unwrap();
        assert_eq!(stale.join().unwrap(), 503);
        assert_eq!(status(&known), 503);
        assert!(issuer.try_recv().unwrap().is_none());
    });
}

fn publish_assertions(service: &Service, collection: &str, assertions: &[(&str, &str, &str)]) {
    let mut db = service.open_collection(collection, false).unwrap();
    let tx = db.transaction_with_behavior(Immediate).unwrap();
    graph::configure(&tx, "synthetic-fixture").unwrap();
    for (subject, object, quote) in assertions {
        let chunk: String = tx.query_row("SELECT c.chunk_id FROM chunks c JOIN passages p ON p.rowid=c.row_id WHERE p.text=?1",
            [quote], |row| row.get(0)).unwrap();
        let assertion =
            json!({"subject":subject,"predicate":"relates_to","object":object,"quote":quote});
        assert!(
            graph::publish(
                &tx,
                &chunk,
                "synthetic-fixture",
                &["relates_to".to_owned()],
                &[assertion]
            )
            .unwrap()
        );
    }
    tx.commit().unwrap();
}

#[test]
fn signed_http_collection_scope_graph_and_policy_floor() {
    let fixture = Fixture::new(None, false);
    let service = fixture.open();
    assert!(Service::open(&fixture.profile).is_err());
    with_http(&service, |address| {
        assert_eq!(api(address, None, "/health", None).0, 200);
        let claims = ADMIN.split('.').nth(1).unwrap();
        let header = |algorithm| {
            URL_SAFE_NO_PAD.encode(json!({"alg":algorithm,"kid":"test","typ":"at+jwt"}).to_string())
        };
        let unsigned = format!("{}.{claims}.", header("none"));
        // DER PKCS#1 public key for the fixture's 2,048-bit modulus and 3-byte exponent.
        let mut public_key = vec![0x30, 0x82, 0x01, 0x0a, 0x02, 0x82, 0x01, 0x01, 0x00];
        public_key.extend(URL_SAFE_NO_PAD.decode(N).unwrap());
        public_key.extend([0x02, 0x03]);
        public_key.extend(URL_SAFE_NO_PAD.decode(E).unwrap());
        let signing_input = format!("{}.{claims}", header("HS256"));
        let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, &public_key);
        let signature = hmac::sign(&hmac_key, signing_input.as_bytes());
        let confused = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature));
        assert_eq!(
            api(address, Some(ADMIN.as_str()), "/v1/admin/status", None).0,
            200
        );
        for forged in [&unsigned, &confused] {
            assert_eq!(api(address, Some(forged), "/v1/admin/status", None).0, 401);
        }
        for token in [
            None,
            Some(WRONG_AUD.as_str()),
            Some(EXPIRED.as_str()),
            Some("invalid.invalid.invalid"),
        ] {
            assert_eq!(
                api(
                    address,
                    token,
                    "/v1/query",
                    Some(json!({"question":"Lyra"}))
                )
                .0,
                401
            );
        }
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                "/v1/admin/sync",
                Some(json!({}))
            )
            .0,
            403
        );
        assert_eq!(sync_http(address)["status"], "complete");
        let bob = query(address, BOB.as_str(), PUBLIC_QUESTION, false);
        assert!(bob.to_string().contains(PRIVATE_CANARY));
        let private_id = bob["evidence"][0]["chunk_id"].as_str().unwrap();
        let secret_url = format!("/v1/evidence/{private_id}");
        assert_eq!(api(address, Some(ALICE.as_str()), &secret_url, None).0, 404);
        assert_eq!(api(address, Some(BOB.as_str()), &secret_url, None).0, 200);
        let alice = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        assert_eq!(alice["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(alice["evidence"][0]["path"], "seed.md");
        assert!(!alice.to_string().contains(PRIVATE_CANARY));
        publish_assertions(
            &service,
            "open",
            &[
                (
                    "Lyra beacon",
                    "Relay Point",
                    "Lyra beacon links Relay Point.",
                ),
                (
                    "Relay Point",
                    "Archive Vault",
                    "Relay Point connects Archive Vault.",
                ),
            ],
        );
        publish_assertions(
            &service,
            "restricted",
            &[(
                "Relay Point",
                PRIVATE_CANARY,
                &format!("Lyra beacon and Relay Point identify {PRIVATE_CANARY}."),
            )],
        );
        let graphed = query(address, ALICE.as_str(), PUBLIC_QUESTION, true);
        assert!(graphed.to_string().contains("Archive Vault"), "{graphed}");
        assert!(!graphed.to_string().contains(PRIVATE_CANARY));
        let (_, vectors) = api(
            address,
            Some(ALICE.as_str()),
            "/v1/query",
            Some(json!({"question":PUBLIC_QUESTION,"mode":"hybrid"})),
        );
        assert_eq!(vectors["vector_coverage"]["indexed_chunks"], 2);
        assert!(!vectors.to_string().contains(PRIVATE_CANARY));
        let visible = alice["evidence"][0]["chunk_id"].as_str().unwrap();
        fixture.policy(2, false, false, false);
        assert_eq!(
            query(address, ALICE.as_str(), PUBLIC_QUESTION, true)["status"],
            "abstained"
        );
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                &format!("/v1/evidence/{visible}"),
                None
            )
            .0,
            404
        );
        fixture.rewind(1);
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":"Lyra"}))
            )
            .0,
            503
        );
    });
    drop(service);
    assert!(
        Service::open(&fixture.profile).is_err(),
        "policy rollback must fail after restart"
    );
}

fn model_input(request: &mut tiny_http::Request) -> Value {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(64 * 1024)
        .read_to_end(&mut body)
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn model_reply(request: tiny_http::Request, response: Value) {
    request
        .respond(
            tiny_http::Response::from_string(response.to_string()).with_header(
                tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
            ),
        )
        .unwrap();
}

const COMPANY_PROSE: [(&str, &str); 2] = [
    ("BR-592 depends on DZ-326.", "DZ-326"),
    ("BR-592 depends on DZ-327.", "DZ-327"),
];

fn sync_company_graph(address: &str, model: &tiny_http::Server) -> Value {
    thread::scope(|scope| {
        let sync = scope.spawn(|| sync_http(address));
        while !sync.is_finished() {
            let Some(mut request) = model.recv_timeout(Duration::from_millis(20)).unwrap() else {
                continue;
            };
            assert_eq!(request.url(), "/v1/chat/completions");
            let body = model_input(&mut request);
            let input: Value =
                serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
            let passage = input["passage"].as_str().unwrap();
            assert!(!passage.contains(PRIVATE_CANARY));
            let assertions: Vec<_> = COMPANY_PROSE
                .iter()
                .filter(|(quote, _)| passage.contains(*quote))
                .map(|(quote, object)| {
                    json!({"subject":"BR-592","predicate":"depends_on","object":object,"quote":quote})
                })
                .collect();
            model_reply(
                request,
                json!({"choices":[{"finish_reason":"stop","message":{
                    "content":json!({"assertions":assertions}).to_string()
                }}]}),
            );
        }
        sync.join().unwrap()
    })
}

fn graph_evidence<'a>(result: &'a Value, path: &str) -> Option<&'a Value> {
    result["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["path"] == path && item["graph_support"]["assertion"].is_object())
}

fn assert_proposal_quote(proposal: &Value, source: &str, expected: &str) {
    assert_eq!(proposal["quote"], expected, "{proposal}");
    let start = proposal["start"].as_u64().unwrap() as usize;
    let end = proposal["end"].as_u64().unwrap() as usize;
    assert!(start < end && end <= source.chars().count(), "{proposal}");
    assert_eq!(
        source
            .chars()
            .skip(start)
            .take(end - start)
            .collect::<String>(),
        expected
    );
}

fn company_graph_decision(proposal: &Value, decision: &str, key: &str) -> Value {
    json!({"collection":"open","proposal_id":proposal["proposal_id"],
        "decision":decision,"revision":proposal["revision"],"fingerprint":proposal["fingerprint"],
        "policy_version":proposal["policy_version"],"idempotency_key":key})
}

#[test]
fn signed_http_company_graph_publication_review_and_restore() {
    const LIST: &str = "/v1/graph-review/list";
    const DECIDE: &str = "/v1/graph-review/decide";
    const QUESTION: &str = "AX-421";
    const ROUTINE: &str = "BR-592 | requires | FV-684";
    const HIGH_IMPACT: &str = "BR-592 | owned_by | CZ-763";
    const UNTRUSTED: &str = "BR-592 | depends_on | HW-257";
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), false);
    let mut profile: Value = serde_json::from_slice(&fs::read(&fixture.profile).unwrap()).unwrap();
    profile["graph"]["extractor"] = json!("chat");
    fs::write(&fixture.profile, profile.to_string()).unwrap();
    let declaration = |line| format!("# Relations\n\n```anyrag-relations\n{line}\n```\n");
    let high_impact_doc = declaration(HIGH_IMPACT);
    let untrusted_doc = declaration(UNTRUSTED);
    let prose_doc = |index: usize| format!("# Café\n\n{}\n", COMPANY_PROSE[index].0);
    let source = fixture.root.path().join("open");
    fs::write(
        source.join("seed.md"),
        declaration("AX-421 | depends_on | BR-592"),
    )
    .unwrap();
    fs::write(source.join("routine.md"), declaration(ROUTINE)).unwrap();
    fs::write(source.join("routine-copy.md"), &untrusted_doc).unwrap();
    fs::write(source.join("hop.md"), &high_impact_doc).unwrap();
    fs::write(source.join("prose.md"), prose_doc(0)).unwrap();
    let mut policy = json!({"schema_version":1,"version":2,"issuer":ISSUER,
        "administrators":["admin"],"subjects":{"alice":["reader","reviewer"]},
        "collections":{"open":{"roles":["reader"],"subjects":["bob"],"endpoints":["chat"],
            "graph_review_roles":["reviewer"],"graph_review_subjects":["admin"],
            "graph_publication":{"trusted_paths":["seed.md","routine.md","hop.md","conflict.md"],
                "routine_predicates":["depends_on","requires"],"high_impact_predicates":["owned_by"],
                "single_valued_predicates":["requires"]}},
            "restricted":{"subjects":["bob"]}}});
    fixture.write_policy(policy.clone());
    let service = fixture.open();
    with_http(&service, |address| {
        assert_eq!(sync_company_graph(address, &model)["status"], "complete");
        let read_list =
            |token: Option<&str>| api(address, token, LIST, Some(json!({"collection":"open"})));
        let list = || {
            let (status, result) = read_list(Some(ALICE.as_str()));
            assert_eq!(status, 200, "{result}");
            result["proposals"].as_array().unwrap().clone()
        };
        let decide = |token: &str, body| api(address, Some(token), DECIDE, Some(body));
        assert_eq!(read_list(None).0, 401);
        assert_eq!(
            query(address, BOB.as_str(), QUESTION, false)["status"],
            "evidence"
        );
        assert_eq!(read_list(Some(BOB.as_str())).0, 403, "read-only subject");
        assert_eq!(
            read_list(Some(ADMIN.as_str())).0,
            403,
            "review without read"
        );

        let initial = query(address, ALICE.as_str(), QUESTION, true);
        let routine = graph_evidence(&initial, "routine.md").expect("trusted routine graph hop");
        assert_eq!(routine["graph_support"]["assertion"]["quote"], ROUTINE);
        assert_eq!(routine["graph_support"]["assertion_status"], "declared");
        assert_eq!(
            routine["graph_support"]["path_contains_unverified_assertion"],
            false
        );
        let citation = format!("/v1/evidence/{}", routine["chunk_id"].as_str().unwrap());
        let (status, reopened) = api(address, Some(ALICE.as_str()), &citation, None);
        assert_eq!(status, 200, "{reopened}");
        assert_eq!(reopened["evidence"]["revision"], routine["revision"]);
        assert!(graph_evidence(&initial, "hop.md").is_none());
        assert!(graph_evidence(&initial, "routine-copy.md").is_none());
        assert!(graph_evidence(&initial, "prose.md").is_none());
        assert!(!initial.to_string().contains(PRIVATE_CANARY));

        let pending = list();
        assert_eq!(pending.len(), 3, "{pending:?}");
        let high = pending
            .iter()
            .find(|item| item["path"] == "hop.md")
            .unwrap();
        let prose = pending
            .iter()
            .find(|item| item["path"] == "prose.md")
            .unwrap();
        let untrusted = pending
            .iter()
            .find(|item| item["path"] == "routine-copy.md")
            .unwrap();
        assert_eq!(high["reason"], "high_impact_predicate");
        assert_eq!(prose["reason"], "model_only");
        assert_eq!(untrusted["reason"], "untrusted_declaration_path");
        assert_proposal_quote(high, &high_impact_doc, HIGH_IMPACT);
        assert_proposal_quote(prose, &prose_doc(0), COMPANY_PROSE[0].0);
        assert_proposal_quote(untrusted, &untrusted_doc, UNTRUSTED);
        for proposal in [high, prose, untrusted] {
            assert_eq!(proposal["policy_version"], 2);
            for field in ["proposal_id", "revision", "fingerprint"] {
                assert!(
                    proposal[field]
                        .as_str()
                        .is_some_and(|value| !value.is_empty())
                );
            }
        }
        let approve = company_graph_decision(high, "approve", "review-high-impact-1");
        for token in [BOB.as_str(), ADMIN.as_str()] {
            assert_eq!(decide(token, approve.clone()).0, 403);
        }
        let receipt = decide(ALICE.as_str(), approve.clone());
        assert_eq!(receipt.0, 200, "{}", receipt.1);
        assert_eq!(decide(ALICE.as_str(), approve.clone()), receipt);
        let mut conflict = approve;
        conflict["decision"] = json!("dismiss");
        assert_eq!(decide(ALICE.as_str(), conflict).0, 409);
        let dismiss = company_graph_decision(untrusted, "dismiss", "dismiss-untrusted-1");
        let dismissal = decide(ALICE.as_str(), dismiss.clone());
        assert_eq!(dismissal.0, 200, "{}", dismissal.1);
        assert_eq!(decide(ALICE.as_str(), dismiss), dismissal);
        let approved = query(address, ALICE.as_str(), QUESTION, true);
        let reviewed = &graph_evidence(&approved, "hop.md").unwrap()["graph_support"];
        assert_eq!(reviewed["assertion"]["quote"], HIGH_IMPACT);
        assert_eq!(reviewed["assertion_status"], "reviewed");
        assert_eq!(reviewed["path_contains_unverified_assertion"], false);
        assert!(graph_evidence(&approved, "prose.md").is_none());
        assert!(graph_evidence(&approved, "routine-copy.md").is_none());
        assert_eq!(list(), vec![(*prose).clone()]);

        // The existing source bytes and policy are unchanged when a second
        // declaration makes this single-valued relationship ambiguous.
        let conflicting_source = source.join("conflict.md");
        fs::write(
            &conflicting_source,
            declaration("BR-592 | requires | GE-195"),
        )
        .unwrap();
        assert_eq!(sync_company_graph(address, &model)["status"], "complete");
        let conflicts = list();
        assert_eq!(conflicts.len(), 3, "{conflicts:?}");
        let demoted = query(address, ALICE.as_str(), QUESTION, true);
        for path in ["routine.md", "conflict.md"] {
            assert!(conflicts.iter().any(|item| item["path"] == path
                && item["reason"] == "conflicting_declaration"));
            assert!(graph_evidence(&demoted, path).is_none());
        }
        fs::remove_file(conflicting_source).unwrap();
        assert_eq!(sync_company_graph(address, &model)["status"], "complete");
        assert_eq!(list(), vec![(*prose).clone()]);
        let recovered = query(address, ALICE.as_str(), QUESTION, true);
        assert_eq!(
            graph_evidence(&recovered, "routine.md").unwrap()["graph_support"]["assertion"]["quote"],
            ROUTINE
        );
        assert!(graph_evidence(&recovered, "hop.md").is_some());

        let (status, backup) = api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/backup",
            Some(json!({})),
        );
        assert_eq!(status, 200, "{backup}");
        let old_prose = company_graph_decision(prose, "approve", "review-old-prose-1");
        fs::write(source.join("prose.md"), prose_doc(1)).unwrap();
        assert_eq!(
            decide(ALICE.as_str(), old_prose.clone()).0,
            409,
            "raw source edit"
        );
        assert_eq!(sync_company_graph(address, &model)["status"], "complete");
        assert_eq!(decide(ALICE.as_str(), old_prose).0, 409, "retired revision");
        let revised = list();
        assert_eq!(revised.len(), 1, "{revised:?}");
        assert_ne!(revised[0]["revision"], prose["revision"]);
        assert_proposal_quote(&revised[0], &prose_doc(1), COMPANY_PROSE[1].0);

        let (status, restored) = api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/restore",
            Some(json!({"backup_id":backup["backup_id"]})),
        );
        assert_eq!(status, 200, "{restored}");
        assert_eq!(read_list(Some(ALICE.as_str())).0, 503);
        assert_eq!(sync_company_graph(address, &model)["status"], "complete");
        let (status, reopened) = api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/reopen",
            Some(json!({})),
        );
        assert_eq!(status, 200, "{reopened}");
        let after_restore = list();
        assert_eq!(after_restore.len(), 1, "{after_restore:?}");
        assert_proposal_quote(&after_restore[0], &prose_doc(1), COMPANY_PROSE[1].0);
        let restored_graph = query(address, ALICE.as_str(), QUESTION, true);
        for (path, status) in [("routine.md", "declared"), ("hop.md", "reviewed")] {
            let graph_support = &graph_evidence(&restored_graph, path).unwrap()["graph_support"];
            assert_eq!(graph_support["assertion_status"], status);
            assert_eq!(graph_support["path_contains_unverified_assertion"], false);
        }

        policy["version"] = json!(3);
        policy["collections"]["open"]["subjects"] = json!(["bob", "admin"]);
        policy["collections"]["open"]["graph_review_subjects"] = json!([]);
        policy["collections"]["open"]["graph_publication"]["trusted_paths"] = json!(["seed.md"]);
        fixture.write_policy(policy);
        assert_eq!(
            query(address, ADMIN.as_str(), QUESTION, false)["status"],
            "evidence"
        );
        assert_eq!(
            read_list(Some(ADMIN.as_str())).0,
            403,
            "admin read without review"
        );
        assert_eq!(
            read_list(Some(ALICE.as_str())).0,
            409,
            "reconcile current rules first"
        );
        assert_eq!(
            decide(
                ALICE.as_str(),
                company_graph_decision(&after_restore[0], "approve", "review-old-policy-1")
            )
            .0,
            409
        );
        let stale = query(address, ALICE.as_str(), QUESTION, true);
        assert_eq!(stale["status"], "evidence");
        assert!(graph_evidence(&stale, "routine.md").is_none());
        assert!(graph_evidence(&stale, "hop.md").is_none());
    });
}

#[test]
fn slow_sync_reports_retryable_busy_and_replay_keeps_one_revision() {
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), true);
    let mut policy: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("policy/current.json")).unwrap())
            .unwrap();
    policy["version"] = json!(2);
    policy["collections"]["restricted"]["endpoints"] = json!(["embedding"]);
    fixture.write_policy(policy);
    let service = fixture.open();
    let mut db = service.open_collection("open", false).unwrap();
    service.sources["open"]
        .ingest(&mut db, false, &mut store::OcrBatch::new(None))
        .unwrap();
    fs::write(
        fixture.root.path().join("open/seed.md"),
        "Lyra beacon links Relay Point. Revised.",
    )
    .unwrap();
    with_http(&service, |address| {
        let old = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        let old_chunk = old["evidence"][0]["chunk_id"].as_str().unwrap();
        let sync_path = "/v1/admin/sync";
        let question = Some(json!({"question":PUBLIC_QUESTION}));
        assert_eq!(
            api(address, Some(ADMIN.as_str()), sync_path, Some(json!({}))).0,
            202
        );
        let mut stalled = model.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        assert_eq!(stalled.url(), "/v1/embeddings");
        assert!(
            !model_input(&mut stalled)
                .to_string()
                .contains(PRIVATE_CANARY)
        );
        let mut worst = Duration::ZERO;
        let citation = format!("/v1/evidence/{old_chunk}");
        for (token, path, payload) in [
            (ALICE.as_str(), "/v1/query", question.clone()),
            (ALICE.as_str(), citation.as_str(), None),
            (ADMIN.as_str(), sync_path, Some(json!({}))),
        ] {
            let start = Instant::now();
            let (status, body) = api(address, Some(token), path, payload);
            worst = worst.max(start.elapsed());
            assert_eq!(status, 409, "{body}");
            assert_eq!(body["error"]["code"], "WRITER_BUSY");
            assert_eq!(body["error"]["retryable"], true);
            assert_eq!(body["error"]["retry_after_seconds"], 1);
        }
        assert_eq!(
            api(address, Some(ADMIN.as_str()), "/v1/admin/status", None).1["sync"]["status"],
            "running"
        );
        assert!(
            worst < Duration::from_secs(1),
            "busy replies took {worst:?}"
        );
        println!("Slow sync: worst query, citation or sync admission busy reply {worst:?}");
        stalled
            .respond(
                tiny_http::Response::from_string(PRIVATE_CANARY)
                    .with_status_code(429)
                    .with_header(tiny_http::Header::from_bytes("Retry-After", "37").unwrap()),
            )
            .unwrap();
        let report = await_sync(address);
        assert_eq!(report["updated"], 2);
        assert_eq!(report["provider_halts"]["vector"], "provider_rate_limited");
        let lane = &report["enrichment"]["open"]["vector"];
        assert_eq!(lane["deferred"], 1);
        assert_eq!(lane["quarantined"], 0);
        assert!(lane["provider_paused_until_unix"].is_i64());
        assert_eq!(report["enrichment"]["restricted"]["vector"]["ready"], 1);
        assert!(!report.to_string().contains(PRIVATE_CANARY));

        let new = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        let evidence = &new["evidence"][0];
        assert_ne!(evidence["chunk_id"], old_chunk);
        let new_chunk = evidence["chunk_id"].as_str().unwrap();
        let new_citation = format!("/v1/evidence/{new_chunk}");
        let (status, cited) = api(address, Some(ALICE.as_str()), &new_citation, None);
        assert_eq!(status, 200);
        assert_eq!(cited["evidence"]["revision"], evidence["revision"]);
        assert_eq!(api(address, Some(ALICE.as_str()), &citation, None).0, 404);

        let retry = "/v1/admin/retry-enrichment";
        let lane = json!({"collection":"open","lane":"vector"});
        assert_eq!(
            api(address, Some(ALICE.as_str()), retry, Some(lane.clone())).0,
            403
        );
        let (status, reset) = api(address, Some(ADMIN.as_str()), retry, Some(lane));
        assert_eq!(status, 200, "{reset}");
        assert_eq!(reset["reset"], 1);
        assert_eq!(reset["progress"]["deferred"], 0);
        assert!(reset["progress"]["provider_paused_until_unix"].is_i64());

        let replay = sync_http(address);
        assert_eq!(replay["updated"], 0);
        assert_eq!(replay["provider_halts"]["vector"], "provider_rate_limited");
        assert!(model.try_recv().unwrap().is_none());
        let unchanged = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        assert_eq!(unchanged["evidence"][0]["revision"], evidence["revision"]);
        let revisions: i64 = db
            .query_row(
                "SELECT count(*) FROM chunks WHERE source_id=?1",
                [evidence["source_id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revisions, 1);
    });
}

#[test]
#[cfg(all(feature = "ocr", target_os = "linux"))]
fn stalled_ocr_serves_committed_citations_under_current_grants() {
    use std::{
        io::Write,
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };

    let fixture = Fixture::new(None, false);
    let root = fixture.root.path().canonicalize().unwrap();
    let directory = files::state_directory(&root.join("ocr"), true).unwrap();
    let socket = directory.join("worker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let profile = directory.join("worker.json");
    fs::write(
        &profile,
        json!({"schema_version":1,"socket":socket,"fingerprint":"a".repeat(64)}).to_string(),
    )
    .unwrap();
    let mut config: Value = serde_json::from_slice(&fs::read(&fixture.profile).unwrap()).unwrap();
    config["ocr_profile"] = json!(profile);
    fs::write(&fixture.profile, config.to_string()).unwrap();

    let (entered, stalled) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        for step in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut hello = [0; 72];
            stream.read_exact(&mut hello).unwrap();
            let mut size = [0; 4];
            stream.read_exact(&mut size).unwrap();
            stream.write_all(&hello).unwrap();
            let count = u32::from_be_bytes(size) as usize;
            if step == 0 {
                assert_eq!(count, 0);
                continue;
            }
            assert!(count < 100);
            stream.read_exact(&mut vec![0; count]).unwrap();
            if step == 2 {
                entered.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if step == 3 {
                break;
            }
            let text = if step == 1 {
                "Lyra beacon committed PDF."
            } else {
                "Lyra beacon uncommitted PDF."
            };
            let result = serde_json::to_vec(&json!({"version":1,"pages":[{
                "number":1,"crop_box_mpt":[0,0,612000,792000],"pdf_rotation_cw":0,"method":"raster_ocr",
                "regions":[{"rect":[1000,1000,9000,9000],"text":text}],"tables":[]
            }]})).unwrap();
            stream
                .write_all(&(result.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&result).unwrap();
        }
    });

    let pdf = root.join("open/synthetic.pdf");
    fs::write(&pdf, b"%PDF fixture revision one").unwrap();
    let service = fixture.open();
    with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete");
        let committed = query(address, ALICE.as_str(), PUBLIC_QUESTION, false)["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row.get("ocr").is_some())
            .unwrap()
            .clone();
        let id = committed["chunk_id"].as_str().unwrap();
        let citation = format!("/v1/evidence/{id}");
        fs::write(&pdf, b"%PDF fixture revision two").unwrap();
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/sync",
                Some(json!({}))
            )
            .0,
            202
        );
        stalled.recv_timeout(Duration::from_secs(3)).unwrap();

        let during = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        assert!(
            during["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["chunk_id"] == id)
        );
        let (status, saved) = api(address, Some(ALICE.as_str()), &citation, None);
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["evidence"]["ocr"], committed["ocr"]);
        for action in ["sync", "backup", "reopen"] {
            let (status, body) = api(
                address,
                Some(ADMIN.as_str()),
                &format!("/v1/admin/{action}"),
                Some(json!({})),
            );
            assert_eq!(status, 409, "{body}");
            assert_eq!(body["error"]["code"], "WRITER_BUSY");
        }

        fixture.policy(2, false, false, false);
        assert_eq!(api(address, Some(ALICE.as_str()), &citation, None).0, 404);
        assert_eq!(
            query(address, ALICE.as_str(), PUBLIC_QUESTION, false)["evidence"],
            json!([])
        );
        release.send(()).unwrap();
        let interrupted = await_sync(address);
        assert_eq!(interrupted["status"], "incomplete");
        assert_eq!(interrupted["reason"], ACCESS_CHANGED.1);

        fixture.policy(3, true, false, false);
        let failed = sync_http(address);
        assert_eq!(failed["status"], "partial");
        assert_eq!(failed["failed"], 1);
        let (_, retained) = api(address, Some(ALICE.as_str()), &citation, None);
        assert_eq!(retained["evidence"]["revision"], committed["revision"]);
        let after = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        assert!(!after.to_string().contains("uncommitted PDF"));
    });
    worker.join().unwrap();
}

#[test]
fn provider_faults_return_scoped_fallback_and_redact_credentials() {
    use std::io::Write;

    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), true);
    let token = "synthetic-test-credential";
    let credential = fixture.root.path().join("credential.fixture");
    new_private(&credential)
        .unwrap()
        .write_all(token.as_bytes())
        .unwrap();
    let config_path = fixture.root.path().join("answer.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["api_key_file"] = json!(credential);
    config["timeout_seconds"] = json!(1);
    fs::write(config_path, config.to_string()).unwrap();
    let service = fixture.open();
    for (name, source) in &service.sources {
        let mut db = service.open_collection(name, false).unwrap();
        source
            .ingest(&mut db, false, &mut store::OcrBatch::new(None))
            .unwrap();
    }
    let receive = || model.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    with_http(&service, |address| {
        let chat = || {
            api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":PUBLIC_QUESTION,"answer_endpoint":"chat"})),
            )
        };
        let duplicate_inner = json!({"choices":[{"finish_reason":"stop","message":{"content":
            r#"{"status":"insufficient_evidence","status":"answered","answer":null,"citations":[]}"#}}]}).to_string();
        for (reply, status, reason) in [
            (format!("{{{token}"), 200, "provider_unavailable"),
            (
                r#"{"choices":[],"choices":[]}"#.into(),
                200,
                "provider_unavailable",
            ),
            ("x".repeat(256 * 1024 + 1), 200, "provider_unavailable"),
            (duplicate_inner, 200, "invalid_item"),
            (
                format!("{token} {PRIVATE_CANARY}"),
                429,
                "provider_rate_limited",
            ),
        ] {
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut request = receive();
                    assert_eq!(request.url(), "/v1/chat/completions");
                    let auth = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .unwrap();
                    assert!(auth.value.as_str().ends_with(token));
                    assert!(
                        !model_input(&mut request)
                            .to_string()
                            .contains(PRIVATE_CANARY)
                    );
                    request
                        .respond(tiny_http::Response::from_string(reply).with_status_code(status))
                        .unwrap();
                });
                let (status, result) = chat();
                assert_eq!(status, 200);
                assert_eq!(result["status"], "evidence");
                assert_eq!(result["answer"]["status"], "unavailable");
                assert_eq!(result["answer"]["reason"], reason);
                assert_eq!(result["answer_generated"], false);
                let raw = result.to_string();
                assert!(!raw.contains(token));
                assert!(!raw.contains(PRIVATE_CANARY));
            });
        }
        thread::scope(|scope| {
            let (finished, release) = mpsc::channel();
            scope.spawn(move || {
                let mut request = receive();
                model_input(&mut request);
                // Hold the socket open until the client's configured timeout.
                release.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            let start = Instant::now();
            let (status, result) = chat();
            finished.send(()).unwrap();
            assert_eq!(status, 200);
            assert_eq!(result["answer"]["reason"], "provider_unavailable");
            assert!(start.elapsed() < Duration::from_secs(3));
        });
        for mode in ["hybrid", "vector"] {
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut request = receive();
                    assert_eq!(request.url(), "/v1/embeddings");
                    assert_eq!(model_input(&mut request)["input"], PUBLIC_QUESTION);
                    request
                        .respond(
                            tiny_http::Response::from_string(PRIVATE_CANARY)
                                .with_status_code(429)
                                .with_header(
                                    tiny_http::Header::from_bytes("Retry-After", "37").unwrap(),
                                ),
                        )
                        .unwrap();
                });
                let (status, result) = api(
                    address,
                    Some(ALICE.as_str()),
                    "/v1/query",
                    Some(json!({"question":PUBLIC_QUESTION,"mode":mode})),
                );
                assert!(!result.to_string().contains(PRIVATE_CANARY));
                if mode == "hybrid" {
                    assert_eq!(status, 200);
                    assert_eq!(result["mode"], "hybrid");
                    assert_eq!(result["effective_mode"], "lexical");
                    assert_eq!(result["vector_coverage"]["reason"], "provider_rate_limited");
                    assert_eq!(result["status"], "evidence");
                } else {
                    assert_eq!(status, 503);
                    assert_eq!(result["error"]["code"], "MODEL_RATE_LIMITED");
                    assert_eq!(result["error"]["retry_after_seconds"], 37);
                }
            });
        }
    });
}

#[test]
fn shared_answer_delivers_once_and_accepts_only_sent_citations() {
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), false);
    let service = fixture.open();
    let ask = |address: &str, cite: &(dyn Fn(&[Value]) -> Vec<String> + Sync)| {
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut request = model.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
                let body = model_input(&mut request);
                let content = body["messages"][1]["content"].as_str().unwrap().to_owned();
                let input: Value = serde_json::from_str(&content).unwrap();
                let sent = input["evidence"].as_array().unwrap().clone();
                for item in &sent {
                    let passage = item["passage"].as_str().unwrap();
                    assert_eq!(content.matches(passage).count(), 1, "{passage}");
                }
                assert!(!body.to_string().contains(PRIVATE_CANARY));
                let answer = json!({"status":"answered","answer":"Synthetic answer",
                    "citations":cite(&sent)})
                .to_string();
                model_reply(
                    request,
                    json!({"choices":[{"finish_reason":"stop","message":{"content":answer}}]}),
                );
                sent
            });
            let (status, result) = api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":"Relay Point","answer_endpoint":"chat"})),
            );
            assert_eq!(status, 200, "{result}");
            (worker.join().unwrap(), result)
        })
    };
    with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete");
        let denied = query(address, BOB.as_str(), PUBLIC_QUESTION, false);
        let denied_id = denied["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["collection"] == "restricted")
            .unwrap()["chunk_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // A real chunk ID the caller may not read was never sent: rejected.
        let (sent, result) = ask(address, &|_| vec![denied_id.clone()]);
        assert!(!sent.is_empty());
        assert_eq!(result["answer"]["status"], "unavailable");
        assert_eq!(result["answer"]["reason"], "invalid_item");
        assert_eq!(result["answer_generated"], false);
        assert!(!result.to_string().contains(PRIVATE_CANARY));
        let delivery = &result["answer"]["delivery"];
        assert_eq!(delivery["sent"].as_array().unwrap().len(), sent.len());
        assert!(
            delivery["sent"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["collection"] == "open" && item.get("text").is_none())
        );
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                &format!("/v1/evidence/{denied_id}"),
                None
            )
            .0,
            404
        );

        // A two-source answer resolves to canonical collection/source/revision/range.
        let (sent, result) = ask(address, &|sent| {
            sent.iter()
                .map(|item| item["id"].as_str().unwrap().to_owned())
                .collect()
        });
        assert_eq!(result["answer"]["status"], "answered", "{result}");
        let cited = result["answer"]["cited_evidence"].as_array().unwrap();
        assert_eq!(cited.len(), sent.len());
        assert!(
            cited
                .iter()
                .map(|item| item["source_id"].as_str().unwrap())
                .collect::<BTreeSet<_>>()
                .len()
                >= 2
        );
        for (item, sent) in cited.iter().zip(&sent) {
            assert_eq!(item["chunk_id"], sent["id"]);
            let (status, reopened) = api(
                address,
                Some(ALICE.as_str()),
                &format!("/v1/evidence/{}", item["chunk_id"].as_str().unwrap()),
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(reopened["evidence"]["text"], sent["passage"]);
            for key in ["collection", "source_id", "revision", "start", "end"] {
                assert_eq!(reopened["evidence"][key], item[key], "{key}");
            }
        }

        // The answer holds the read gate from retrieval through the reply, so
        // a sync cannot retire a sent revision before citations resolve.
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut request = model.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
                let body = model_input(&mut request);
                // The test HTTP loop is serial, so admit the sync directly.
                let authorization = format!("Bearer {}", ADMIN.as_str());
                let admin = service
                    .auth
                    .authenticate(Some(&authorization))
                    .ok()
                    .unwrap();
                let busy = service.start_sync(admin).unwrap_err();
                assert_eq!((busy.0, busy.1), (409, "WRITER_BUSY"));
                let content = body["messages"][1]["content"].as_str().unwrap();
                let input: Value = serde_json::from_str(content).unwrap();
                let labels: Vec<_> = input["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["id"].clone())
                    .collect();
                let answer = json!({"status":"answered","answer":"Synthetic answer",
                    "citations":labels})
                .to_string();
                model_reply(
                    request,
                    json!({"choices":[{"finish_reason":"stop","message":{"content":answer}}]}),
                );
            });
            let (status, result) = api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":"Relay Point","answer_endpoint":"chat"})),
            );
            worker.join().unwrap();
            assert_eq!(status, 200, "{result}");
            assert_eq!(result["answer"]["status"], "answered", "{result}");
        });

        // Editing the source retires the cited revision for direct reopening.
        let retired = cited
            .iter()
            .find(|item| {
                api(
                    address,
                    Some(ALICE.as_str()),
                    &format!("/v1/evidence/{}", item["chunk_id"].as_str().unwrap()),
                    None,
                )
                .1["evidence"]["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("hop.md"))
            })
            .unwrap()["chunk_id"]
            .as_str()
            .unwrap()
            .to_owned();
        fs::write(
            fixture.root.path().join("open/hop.md"),
            "Relay Point now connects North Vault.",
        )
        .unwrap();
        assert_eq!(sync_http(address)["status"], "complete");
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                &format!("/v1/evidence/{retired}"),
                None
            )
            .0,
            404
        );
    });
}

#[test]
fn model_input_excludes_denied_graph_and_inflight_revocation_discards_answer() {
    for short in [false, true] {
        check_inflight_answer_revocation(short);
    }
}

fn check_inflight_answer_revocation(short: bool) {
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), false);
    if short {
        let path = fixture.root.path().join("answer.json");
        let mut profile: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        profile["short_answer_citations"] = json!(true);
        fs::write(path, profile.to_string()).unwrap();
    }
    let service = fixture.open();
    with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete");
        if short {
            thread::scope(|scope| {
                scope.spawn(|| {
                    let request = model.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
                    let answer =
                        json!({"status":"answered","answer":"Synthetic answer","citations":["E999"]})
                            .to_string();
                    model_reply(
                        request,
                        json!({"choices":[{"finish_reason":"stop","message":{"content":answer}}]}),
                    );
                });
                let (status, result) = api(
                    address,
                    Some(ALICE.as_str()),
                    "/v1/query",
                    Some(json!({"question":PUBLIC_QUESTION,"answer_endpoint":"chat"})),
                );
                assert_eq!(status, 200, "{result}");
                assert_eq!(result["answer"]["status"], "unavailable");
                assert_eq!(result["answer"]["reason"], "invalid_item");
                assert_eq!(result["answer_generated"], false);
                assert_eq!(
                    result["evidence"][0]["chunk_id"].as_str().unwrap().len(),
                    64
                );
                assert!(!result.to_string().contains("Synthetic answer"));
            });
        }
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut request = model.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
                let body = model_input(&mut request);
                let input: Value =
                    serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                let selected = input["evidence"].as_array().unwrap();
                assert_eq!(selected.len(), 1);
                assert!(!input.to_string().contains(PRIVATE_CANARY));
                let id = selected[0]["id"].as_str().unwrap();
                if short {
                    assert_eq!(id, "E1");
                } else {
                    assert_eq!(id.len(), 64);
                }
                fixture.policy(2, false, true, false);
                let answer =
                    json!({"status":"answered","answer":"Synthetic answer","citations":[id]})
                        .to_string();
                model_reply(
                    request,
                    json!({"choices":[{"finish_reason":"stop","message":{"content":answer}}]}),
                );
            });
            let (status, result) = api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":PUBLIC_QUESTION,"answer_endpoint":"chat"})),
            );
            assert_eq!(status, 409, "{result}");
            assert!(!result.to_string().contains("Synthetic answer"));
        });
        let (status, result) = api(
            address,
            Some(ALICE.as_str()),
            "/v1/query",
            Some(json!({"question":PUBLIC_QUESTION,"answer_endpoint":"chat"})),
        );
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["answer"]["status"], "insufficient_evidence");
        let (status, result) = api(
            address,
            Some(BOB.as_str()),
            "/v1/query",
            Some(json!({"question":PUBLIC_QUESTION,"answer_endpoint":"chat"})),
        );
        assert_eq!(status, 403, "{result}");
        assert!(
            model
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn rerank_boundary_excludes_denied_evidence_and_discards_inflight_result_on_revocation() {
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), false);
    let service = fixture.open();
    with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete")
    });
    let authorization = format!("Bearer {}", ALICE.as_str());
    let principal = service
        .auth
        .authenticate(Some(&authorization))
        .ok()
        .unwrap();
    let request = serde_json::from_value(json!({
        "question":PUBLIC_QUESTION,"limit":20,"graph":false,"rerank":true,"answer_endpoint":"chat"
    }))
    .unwrap();
    let result = service.query_with_reranker(&principal, request, |question, evidence| {
        assert_eq!(question, PUBLIC_QUESTION);
        assert!(!evidence.is_empty());
        assert!(evidence.iter().all(|hit| hit["collection"] == "open"));
        assert!(
            evidence
                .iter()
                .all(|hit| { !hit["text"].as_str().unwrap().contains(PRIVATE_CANARY) })
        );
        fixture.policy(2, false, true, false);
        evidence.reverse();
        json!({"status":"applied"})
    });
    assert!(matches!(result, Err(ApiError(409, "ACCESS_CHANGED", _, _))));
    assert!(
        model
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}

#[test]
fn revoke_destination_between_embedding_and_answer() {
    let model = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let fixture = Fixture::new(Some(&model.server_addr().to_string()), true);
    let service = fixture.open();
    // Populate local evidence without calling an embedding endpoint during setup.
    for (name, source) in &service.sources {
        let mut db = service.open_collection(name, false).unwrap();
        source
            .ingest(&mut db, false, &mut store::OcrBatch::new(None))
            .unwrap();
    }
    with_http(&service, |address| {
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut request = model.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
                assert_eq!(request.url(), "/v1/embeddings");
                assert_eq!(model_input(&mut request)["input"], PUBLIC_QUESTION);
                fixture.policy(2, true, false, true);
                model_reply(request, json!({"data":[{"index":0,"embedding":[1.0,0.0]}]}));
            });
            let (status, body) = api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":PUBLIC_QUESTION,"mode":"hybrid","answer_endpoint":"chat"})),
            );
            assert_eq!(status, 403, "{body}");
            assert_eq!(body["error"]["code"], "MODEL_DESTINATION_DENIED");
        });
        assert!(
            model
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn all_collection_snapshot_interruption_and_reconciliation() {
    let fixture = Fixture::new(None, false);
    let service = fixture.open();
    let (backup, deleted_citation, revoked_citation) = with_http(&service, |address| {
        assert_eq!(sync_http(address)["status"], "complete");
        let (status, body) = api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/backup",
            Some(json!({})),
        );
        assert_eq!(status, 200, "{body}");
        let backup = body["backup_id"].as_str().unwrap().to_owned();
        let open = query(address, ALICE.as_str(), PUBLIC_QUESTION, false);
        let id = open["evidence"][0]["source_id"].as_str().unwrap();
        let deleted_chunk = open["evidence"][0]["chunk_id"].as_str().unwrap();
        let deleted_citation = format!("/v1/evidence/{deleted_chunk}");
        let restricted = query(address, BOB.as_str(), PUBLIC_QUESTION, false);
        assert!(restricted.to_string().contains(PRIVATE_CANARY));
        let revoked_chunk = restricted["evidence"][0]["chunk_id"].as_str().unwrap();
        let revoked_citation = format!("/v1/evidence/{revoked_chunk}");
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/remove",
                Some(json!({"source_id":id}))
            )
            .0,
            200
        );
        assert!(fixture.root.path().join("open/seed.md").exists());
        assert_eq!(
            query(address, ALICE.as_str(), PUBLIC_QUESTION, false)["status"],
            "abstained"
        );
        fs::remove_file(fixture.root.path().join("open/seed.md")).unwrap();
        fixture.policy_with_restricted_reader(2, true, false, false, "admin");
        assert_eq!(
            api(address, Some(BOB.as_str()), &revoked_citation, None).0,
            404
        );
        assert_eq!(sync_http(address)["status"], "complete");
        let saved_file = fixture
            .root
            .path()
            .join("backup")
            .join(&backup)
            .join("restricted.sqlite3");
        let saved_bytes = fs::read(&saved_file).unwrap();
        fs::remove_file(&saved_file).unwrap();
        let rejected_before_copy = || {
            let (status, error) = api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/restore",
                Some(json!({"backup_id":backup})),
            );
            assert_eq!(status, 503, "{error}");
            assert_eq!(error["error"]["code"], "BACKUP_INVALID");
            assert_eq!(service.phase().unwrap(), (false, None, false));
            assert_eq!(
                api(address, Some(ALICE.as_str()), &deleted_citation, None).0,
                404
            );
            assert_eq!(
                query(address, ALICE.as_str(), "Archive Vault", false)["status"],
                "evidence"
            );
        };
        rejected_before_copy();
        drop(new_private(&saved_file).unwrap());
        fs::write(&saved_file, b"synthetic corrupt snapshot").unwrap();
        rejected_before_copy();
        fs::write(&saved_file, saved_bytes).unwrap();

        // Make the second destination unavailable only when the copy attempts
        // its write lock. The first collection really is restored first.
        let mut restricted = service.open_collection("restricted", false).unwrap();
        let blocked = restricted.transaction_with_behavior(Immediate).unwrap();
        let (status, error) = api(
            address,
            Some(ADMIN.as_str()),
            "/v1/admin/restore",
            Some(json!({"backup_id":backup})),
        );
        assert_eq!(status, 503, "{error}");
        assert_eq!(service.phase().unwrap(), (true, None, true));
        let open = service.open_collection("open", false).unwrap();
        let count: i64 = open
            .query_row("SELECT count(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "first collection restored");
        drop(blocked);
        (backup, deleted_citation, revoked_citation)
    });
    drop(service);
    let restarted = fixture.open();
    with_http(&restarted, |address| {
        assert_eq!(
            api(
                address,
                Some(ALICE.as_str()),
                "/v1/query",
                Some(json!({"question":PUBLIC_QUESTION}))
            )
            .0,
            503
        );
        assert_eq!(sync_http(address)["reason"], "RESTORE_INCOMPLETE");
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/reopen",
                Some(json!({}))
            )
            .0,
            409
        );
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/restore",
                Some(json!({"backup_id":backup}))
            )
            .0,
            200
        );
        assert_eq!(sync_http(address)["status"], "complete");
        fixture.policy_with_restricted_reader(3, true, false, false, "admin");
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/reopen",
                Some(json!({}))
            )
            .0,
            409
        );
        assert_eq!(sync_http(address)["status"], "complete");
        assert_eq!(
            api(
                address,
                Some(ADMIN.as_str()),
                "/v1/admin/reopen",
                Some(json!({}))
            )
            .0,
            200
        );
        assert_eq!(
            query(address, ALICE.as_str(), PUBLIC_QUESTION, true)["status"],
            "abstained"
        );
        assert_eq!(
            query(address, BOB.as_str(), PUBLIC_QUESTION, true)["status"],
            "abstained"
        );
        assert_eq!(
            api(address, Some(ALICE.as_str()), &deleted_citation, None).0,
            404
        );
        assert_eq!(
            api(address, Some(BOB.as_str()), &revoked_citation, None).0,
            404
        );
        let (status, visible) = api(address, Some(ADMIN.as_str()), &revoked_citation, None);
        assert_eq!(status, 200);
        assert!(visible.to_string().contains(PRIVATE_CANARY));
    });
}
