//! Protocol tests.
//!
//! The protocol is exercised against scripted responses, so the whole flow
//! can be verified without a network and without an Apple account. The SRP
//! implementation is checked against a server side computation, which
//! verifies the algebra rather than a recorded value.

use std::sync::{Arc, Mutex};

use icloud_for_linux::icloud::auth::{AppleAuth, SessionData, SignInOutcome};
use icloud_for_linux::icloud::drive::{DriveNode, DriveService};
use icloud_for_linux::icloud::error::Result;
use icloud_for_linux::icloud::srp::SrpClient;
use icloud_for_linux::icloud::transport::{
    Body, CookieJar, HttpRequest, HttpResponse, ScriptedTransport, Transport,
};
use serde_json::{json, Value};

fn response(status: u16, value: Value) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&value)?,
    })
}

fn empty(status: u16) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    })
}

fn raw(status: u16, body: &str) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.as_bytes().to_vec(),
    })
}

// ───────────────────────────── cookies ─────────────────────────────

#[test]
fn cookies_are_matched_per_host() {
    let mut jar = CookieJar::new();
    jar.store(
        "https://idmsa.apple.com/appleauth/auth/signin",
        &["scnt=abc; Domain=.apple.com; Path=/".to_string()],
    );
    jar.store(
        "https://p42-icloud-content.icloud.com/upload",
        &["token=xyz; Domain=.icloud.com; Path=/".to_string()],
    );

    let header = jar
        .header_for("https://idmsa.apple.com/appleauth/auth/signin")
        .unwrap();
    assert!(header.contains("scnt=abc"));
    assert!(!header.contains("token=xyz"));

    let header = jar
        .header_for("https://p42-icloud-content.icloud.com/upload")
        .unwrap();
    assert!(header.contains("token=xyz"));
    assert!(!header.contains("scnt=abc"));
}

#[test]
fn cookies_survive_a_round_trip_through_json() {
    let mut jar = CookieJar::new();
    jar.store(
        "https://icloud.com/",
        &["a=1; Domain=.icloud.com".to_string()],
    );
    let restored = CookieJar::from_json(&jar.to_json());
    assert_eq!(restored.get("a"), Some("1"));
}

// ─────────────────────────────── SRP ───────────────────────────────

/// The 2048 bit group modulus of RFC 5054.
const GROUP_N_HEX: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050",
    "A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50",
    "E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B8",
    "55F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773B",
    "CA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748",
    "544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861602790",
    "04E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8",
    "E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F",
    "9E4AFF73",
);

fn pad(value: &num_bigint::BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut padded = vec![0u8; 256];
    let start = 256usize.saturating_sub(bytes.len());
    padded[start..].copy_from_slice(&bytes[bytes.len().saturating_sub(256)..]);
    padded
}

/// The server side of SRP-6a, used to verify the client.
///
/// The password file holds the verifier `v = g^x`, and the server answers
/// with `B = k*v + g^b`. The client's base is `B - k*g^x = g^b`, so both
/// sides arrive at the same premaster secret.
fn server_proof(
    username: &str,
    password: &str,
    salt: &[u8],
    iterations: u32,
    protocol: &str,
    client_public: &[u8],
    server_secret: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    use num_bigint::BigUint;
    use sha2::{Digest, Sha256};

    let n = BigUint::parse_bytes(GROUP_N_HEX.as_bytes(), 16).unwrap();
    let g = BigUint::from(2u32);
    let hash = |data: &[u8]| -> Vec<u8> { Sha256::digest(data).to_vec() };

    let a = BigUint::from_bytes_be(client_public);
    let b = BigUint::from_bytes_be(server_secret);

    // Password derivation, the same way the client does it.
    let pass_hash = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = match protocol {
        "s2k" => pass_hash.to_vec(),
        _ => hex::encode(pass_hash).into_bytes(),
    };
    let mut derived = vec![0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, iterations, &mut derived);

    // x = H(salt | H(":" | derived)) — Apple omits the username.
    let mut inner = Sha256::new();
    inner.update(b":");
    inner.update(&derived);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(salt);
    outer.update(inner);
    let x = BigUint::from_bytes_be(&outer.finalize());

    // The password verifier and the server's public value.
    let v = g.modpow(&x, &n);
    let k = BigUint::from_bytes_be(&hash(&[n.to_bytes_be().as_slice(), &pad(&g)].concat()));
    let b_public = g.modpow(&b, &n);
    let b_value = (&k * &v + &b_public) % &n;

    // u = H(pad(A) | pad(B))
    let mut u_hasher = Sha256::new();
    u_hasher.update(pad(&a));
    u_hasher.update(pad(&b_value));
    let u = BigUint::from_bytes_be(&u_hasher.finalize());

    // S = (A * v^u)^b
    let shared = (&a * v.modpow(&u, &n)) % &n;
    let shared = shared.modpow(&b, &n);
    let session_key = hash(&pad(&shared));

    // M1 = H(H(g) xor H(N) | H(user) | salt | A | B | K)
    let hg = hash(&pad(&g));
    let hn = hash(&n.to_bytes_be());
    let hu = hash(username.as_bytes());
    let xor: Vec<u8> = hg.iter().zip(hn.iter()).map(|(x, y)| x ^ y).collect();
    let mut m1 = Sha256::new();
    m1.update(xor);
    m1.update(hu);
    m1.update(salt);
    m1.update(pad(&a));
    m1.update(pad(&b_value));
    m1.update(session_key);
    (m1.finalize().to_vec(), b_value.to_bytes_be())
}

#[test]
fn srp_proof_matches_the_server_side_computation() {
    let username = "user@example.com";
    let password = "correct horse battery staple";
    let salt = b"0123456789abcdef";
    let iterations = 20u32;
    let protocol = "s2k";

    let mut client = SrpClient::new().unwrap();
    let client_public = client.public_bytes();

    // The server secret has to be smaller than the group.
    let server_secret = [0x11u8; 32];
    let (expected, server_b) = server_proof(
        username,
        password,
        salt,
        iterations,
        protocol,
        &client_public,
        &server_secret,
    );

    client
        .process_challenge(username, password, salt, &server_b, iterations, protocol)
        .unwrap();

    assert_eq!(client.proof(), expected.as_slice());
}

// ────────────────────────── authentication ──────────────────────────

fn auth_transport() -> (Arc<ScriptedTransport>, Arc<Mutex<CookieJar>>) {
    let transport = Arc::new(ScriptedTransport::new(vec![
        // authorize/signin
        Ok(empty(200).unwrap()),
        // federate
        Ok(empty(200).unwrap()),
        // signin/init
        Ok(response(
            200,
            json!({
                "iteration": 20,
                "salt": "c2FsdHNhbHQ=",
                "protocol": "s2k",
                "b": "////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////Pw==",
                "c": "challenge-token",
            }),
        ).unwrap()),
        // signin/complete
        Ok(response(200, json!({ "authType": "hsa2" })).unwrap()),
        // 2sv/trust
        Ok(empty(200).unwrap()),
        // accountLogin
        Ok(response(
            200,
            json!({
                "dsInfo": { "appleId": "user@example.com" },
                "webservices": {
                    "drivews": { "url": "https://p42-drivews.icloud.com:443" },
                    "docws": { "url": "https://p42-docws.icloud.com:443" },
                },
            }),
        ).unwrap()),
    ]));
    (transport, Arc::new(Mutex::new(CookieJar::new())))
}

#[test]
fn signin_walks_the_whole_flow() {
    let (transport, jar) = auth_transport();
    let mut auth = AppleAuth::new(
        transport.clone(),
        jar,
        "user@example.com",
        "secret",
        SessionData::default(),
        None,
    );

    let outcome = auth.signin().expect("sign in");
    assert!(matches!(outcome, SignInOutcome::Authenticated));

    let requests = transport.requests.lock().unwrap();
    let urls: Vec<String> = requests.iter().map(|r| r.url.clone()).collect();
    assert!(urls[0].contains("/authorize/signin"), "{:?}", urls);
    assert!(urls[1].contains("/federate"), "{:?}", urls);
    assert!(urls[2].contains("/signin/init"), "{:?}", urls);
    assert!(urls[3].contains("/signin/complete"), "{:?}", urls);
    assert!(urls[4].contains("/2sv/trust"), "{:?}", urls);
    assert!(urls[5].contains("/accountLogin"), "{:?}", urls);

    // The complete request must carry the proofs.
    let complete = &requests[3];
    let body = match &complete.body {
        Body::Json(value) => value.clone(),
        other => panic!("the complete request must be json, got {:?}", other),
    };
    assert!(body.get("m1").is_some());
    assert!(body.get("m2").is_some());
    assert_eq!(body.get("c").unwrap(), "challenge-token");

    // The service endpoints must have been picked up.
    let session = auth.session();
    assert!(session.webservices.drivews.is_some());
    assert!(session.webservices.docws.is_some());
}

#[test]
fn a_two_factor_challenge_is_reported() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        Ok(empty(200).unwrap()),
        Ok(empty(200).unwrap()),
        Ok(response(
            200,
            json!({
                "iteration": 20,
                "salt": "c2FsdHNhbHQ=",
                "protocol": "s2k",
                "b": "////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////Pw==",
                "c": "challenge",
            }),
        )
        .unwrap()),
        // 409 means the account wants a verification code
        Ok(empty(409).unwrap()),
        // the list of trusted phone numbers
        Ok(response(
            200,
            json!([{ "id": 1, "pushMode": "sms", "nonFTEU": false }]),
        )
        .unwrap()),
    ]));
    let jar = Arc::new(Mutex::new(CookieJar::new()));
    let mut auth = AppleAuth::new(
        transport,
        jar,
        "user@example.com",
        "secret",
        SessionData::default(),
        None,
    );

    match auth.signin().expect("sign in") {
        SignInOutcome::TwoFactorRequired {
            trusted_phone_numbers,
        } => {
            assert_eq!(trusted_phone_numbers.len(), 1);
            assert_eq!(trusted_phone_numbers[0].id, 1);
        }
        other => panic!("expected a two factor challenge, got {:?}", other),
    }
}

#[test]
fn a_wrong_code_is_reported() {
    let transport = Arc::new(ScriptedTransport::new(vec![Ok(response(
        400,
        json!({ "errorCode": "-21669", "errorMessage": "Incorrect verification code" }),
    )
    .unwrap())]));
    let jar = Arc::new(Mutex::new(CookieJar::new()));
    let mut auth = AppleAuth::new(
        transport,
        jar,
        "user@example.com",
        "secret",
        SessionData::default(),
        None,
    );

    let error = auth.submit_code("000000").unwrap_err();
    assert!(
        matches!(error, icloud_for_linux::icloud::error::Error::InvalidCode),
        "unexpected error: {:?}",
        error
    );
}

#[test]
fn the_partition_is_detected_and_reused() {
    let mut response = empty(200).unwrap();
    response
        .headers
        .push(("x-apple-user-partition".to_string(), "ABCDE".to_string()));
    let transport = Arc::new(ScriptedTransport::new(vec![Ok(response)]));
    let jar = Arc::new(Mutex::new(CookieJar::new()));
    let mut auth = AppleAuth::new(
        transport,
        jar,
        "user@example.com",
        "secret",
        SessionData::default(),
        None,
    );

    assert_eq!(auth.detect_partition().unwrap(), "ABCDE");
    assert_eq!(
        auth.setup_endpoint(),
        "https://pABCDE-setup.icloud.com/setup/ws/1"
    );
}

// ─────────────────────────── drive service ──────────────────────────

fn drive_service(responses: Vec<HttpResponse>) -> (Arc<ScriptedTransport>, DriveService) {
    let transport = Arc::new(ScriptedTransport::new(
        responses.into_iter().map(Ok).collect(),
    ));
    let service = DriveService::new(
        Box::new(TransportProxy(transport.clone())),
        "https://p42-drivews.icloud.com:443".to_string(),
        "https://p42-docws.icloud.com:443".to_string(),
        "client-id".to_string(),
    );
    (transport, service)
}

struct TransportProxy(Arc<ScriptedTransport>);

impl Transport for TransportProxy {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        self.0.execute(request)
    }
}

#[test]
fn a_path_is_resolved_by_the_service() {
    let (transport, service) = drive_service(vec![response(
        200,
        json!([{
            "drivewsid": "FOLDER::com.apple.CloudDocs::file-1",
            "docwsid": "doc-1",
            "name": "report.pdf",
            "type": "FILE",
            "etag": "etag-1",
            "size": 4096,
            "zone": "com.apple.CloudDocs",
            "dateModified": "2026-01-02T03:04:05Z",
        }]),
    )
    .unwrap()]);

    let node = service
        .lookup_path("/Downloads/report.pdf")
        .unwrap()
        .unwrap();
    assert_eq!(node.name, "report.pdf");
    assert_eq!(node.size, 4096);
    assert_eq!(node.etag.as_deref(), Some("etag-1"));
    assert!(!node.is_folder());

    let request = transport.last_request().unwrap();
    assert!(
        request.url.contains("/list/lookup_by_path"),
        "{}",
        request.url
    );
    assert_eq!(request.body_text(), "/Downloads/report.pdf");
}

#[test]
fn a_missing_path_is_not_an_error() {
    let (_transport, service) = drive_service(vec![empty(404).unwrap()]);
    assert!(service.lookup_path("/nope").unwrap().is_none());
}

#[test]
fn a_folder_is_listed() {
    let (transport, service) = drive_service(vec![response(
        200,
        json!({ "drive_item": [
            { "name": "one", "type": "FILE", "drivewsid": "id-1", "size": 1 },
            { "name": "two", "type": "FOLDER", "drivewsid": "id-2" },
        ]}),
    )
    .unwrap()]);

    let children = service
        .list_children("FOLDER::com.apple.CloudDocs::root")
        .unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].name, "one");
    assert!(children[1].is_folder());

    let request = transport.last_request().unwrap();
    assert!(request.url.contains("/v1/enumerate/"), "{}", request.url);
}

#[test]
fn a_file_is_downloaded() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("file.bin");

    let (_transport, service) = drive_service(vec![
        response(
            200,
            json!({ "data_token": { "url": "https://content.icloud.com/file" } }),
        )
        .unwrap(),
        raw(200, "file content").unwrap(),
    ]);

    let node = DriveNode {
        drivewsid: Some("id".to_string()),
        docwsid: Some("doc".to_string()),
        name: "file.bin".to_string(),
        node_type: "FILE".to_string(),
        etag: None,
        zone: Some("com.apple.CloudDocs".to_string()),
        size: 12,
        mtime: 0,
        parent_id: None,
    };

    let size = service.download(&node, &dest).unwrap();
    assert_eq!(size, 12);
    assert_eq!(std::fs::read(&dest).unwrap(), b"file content");
}

#[test]
fn a_file_is_uploaded_in_three_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("upload.bin");
    std::fs::write(&file, b"payload").unwrap();

    let (transport, service) = drive_service(vec![
        // 1. the upload slot
        response(
            200,
            json!([{ "document_id": "doc-new", "url": "https://content.icloud.com/upload" }]),
        )
        .unwrap(),
        // 2. the content
        response(
            200,
            json!({ "singleFile": {
                "fileChecksum": "sig",
                "wrappingKey": "key",
                "referenceChecksum": "ref",
                "size": 7,
            }}),
        )
        .unwrap(),
        // 3. the registration
        response(
            200,
            json!({ "results": [{ "document": {
                "document_id": "doc-new",
                "item_id": "item-new",
                "name": "upload.bin",
                "etag": "etag-new",
                "size": 7,
            }}]}),
        )
        .unwrap(),
    ]);

    let parent = DriveNode {
        drivewsid: Some("parent-id".to_string()),
        docwsid: Some("parent-doc".to_string()),
        name: "Downloads".to_string(),
        node_type: "FOLDER".to_string(),
        etag: None,
        zone: Some("com.apple.CloudDocs".to_string()),
        size: 0,
        mtime: 0,
        parent_id: None,
    };

    let node = service.upload(&parent, &file, "upload.bin").unwrap();
    assert_eq!(node.name, "upload.bin");

    let requests = transport.requests.lock().unwrap();
    assert!(
        requests[0].url.contains("/upload/web"),
        "{}",
        requests[0].url
    );
    assert_eq!(requests[1].url, "https://content.icloud.com/upload");
    assert!(
        requests[2].url.contains("/update/documents"),
        "{}",
        requests[2].url
    );
}

#[test]
fn folders_are_created_renamed_moved_and_trashed() {
    let parent = DriveNode {
        drivewsid: Some("parent-id".to_string()),
        docwsid: Some("parent-doc".to_string()),
        name: "Downloads".to_string(),
        node_type: "FOLDER".to_string(),
        etag: None,
        zone: Some("com.apple.CloudDocs".to_string()),
        size: 0,
        mtime: 0,
        parent_id: None,
    };
    let node = DriveNode {
        drivewsid: Some("id".to_string()),
        docwsid: Some("doc".to_string()),
        name: "file.txt".to_string(),
        node_type: "FILE".to_string(),
        etag: Some("etag".to_string()),
        zone: Some("com.apple.CloudDocs".to_string()),
        size: 1,
        mtime: 0,
        parent_id: Some("parent-id".to_string()),
    };

    let (transport, service) = drive_service(vec![
        response(
            200,
            json!({ "folders": [{ "drivewsid": "new-id", "name": "New", "type": "FOLDER" }] }),
        )
        .unwrap(),
        response(
            200,
            json!({ "items": [{ "drivewsid": "id", "name": "renamed", "etag": "e2" }] }),
        )
        .unwrap(),
        response(
            200,
            json!({ "items": [{ "drivewsid": "id", "name": "file.txt", "etag": "e3" }] }),
        )
        .unwrap(),
        response(200, json!({ "items": [{ "status": "OK" }] })).unwrap(),
    ]);

    let created = service.create_folder(&parent, "New").unwrap();
    assert_eq!(created.name, "New");
    let renamed = service.rename(&node, "renamed").unwrap();
    assert_eq!(renamed.name, "renamed");
    let moved = service.move_to(&node, &parent).unwrap();
    assert_eq!(moved.name, "file.txt");
    service.delete(&node).unwrap();

    let requests = transport.requests.lock().unwrap();
    assert!(requests[0].url.ends_with("/createFolders"));
    assert!(requests[1].url.ends_with("/renameItems"));
    assert!(requests[2].url.ends_with("/moveItems"));
    assert!(requests[3].url.ends_with("/moveItemsToTrash"));
}

#[test]
fn a_service_error_is_reported_with_its_code() {
    let (_transport, service) = drive_service(vec![response(
        409,
        json!({ "errorCode": "CONFLICT", "errorMessage": "the item changed" }),
    )
    .unwrap()]);

    let error = service.list_children("id").unwrap_err();
    assert!(
        matches!(error, icloud_for_linux::icloud::error::Error::Api { .. }),
        "unexpected error: {:?}",
        error
    );
}
