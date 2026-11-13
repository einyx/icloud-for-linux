//! Apple ID authentication.
//!
//! The flow is the one Apple's own web front end uses:
//!
//! 1. `authorize/signin` starts an authentication frame.
//! 2. `federate` submits the account name.
//! 3. `signin/init` starts the SRP exchange and returns the salt, the server
//!    public value and the iteration count.
//! 4. `signin/complete` sends the SRP proofs. The service either accepts the
//!    session or answers `409`, which means a two-factor challenge is
//!    pending.
//! 5. `verify/*` answers the challenge (push code, SMS code).
//! 6. `2sv/trust` turns the session into a trusted one, so the next start
//!    does not need a code.
//! 7. `accountLogin` exchanges the session token for the account
//!    description, which contains the endpoints of the services.
//!
//! Cookies, the session token and the trust token are persisted next to the
//! configuration, so a restart resumes without asking for a code again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::error::{Error, Result};
use super::srp::SrpClient;
use super::transport::{CookieJar, HttpRequest, HttpResponse, Method, Transport};

/// The client identifier the web front end uses.
pub const CLIENT_ID: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9f1d1a815d";
/// Where the sign-in requests go.
pub const AUTH_ENDPOINT: &str = "https://idmsa.apple.com/appleauth/auth";
/// Where the account services live.
pub const SETUP_ENDPOINT: &str = "https://setup.icloud.com/setup/ws/1";
/// The redirect URI that has to match the client identifier.
pub const REDIRECT_URI: &str = "https://www.icloud.com";
/// The user agent the service expects.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) \
     Version/18.3.1 Safari/605.1.15";

/// Endpoints of the services we use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebServices {
    #[serde(default)]
    pub drivews: Option<String>,
    #[serde(default)]
    pub docws: Option<String>,
}

/// Everything that has to survive a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionData {
    pub account_name: String,
    pub client_id: String,
    #[serde(default)]
    pub frame_id: String,
    #[serde(default)]
    pub account_country: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub trust_token: Option<String>,
    #[serde(default)]
    pub scnt: Option<String>,
    #[serde(default)]
    pub partition: Option<String>,
    #[serde(default)]
    pub webservices: WebServices,
}

/// The outcome of a sign-in attempt.
#[derive(Debug, Clone)]
pub enum SignInOutcome {
    /// Signed in, the session is trusted.
    Authenticated,
    /// A two-factor challenge is pending.
    TwoFactorRequired {
        /// The trusted phone numbers, when the service reported them.
        trusted_phone_numbers: Vec<TrustedPhone>,
    },
}

/// A trusted phone number of the account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPhone {
    pub id: i64,
    pub push_mode: Option<String>,
    pub non_fteu: Option<bool>,
}

pub struct AppleAuth {
    transport: Arc<dyn Transport>,
    jar: Arc<Mutex<CookieJar>>,
    username: String,
    password: String,
    data: SessionData,
    cookie_path: Option<PathBuf>,
}

impl AppleAuth {
    pub fn new(
        transport: Arc<dyn Transport>,
        jar: Arc<Mutex<CookieJar>>,
        username: &str,
        password: &str,
        data: SessionData,
        cookie_path: Option<PathBuf>,
    ) -> Self {
        let mut data = data;
        if data.client_id.is_empty() {
            data.client_id = new_client_id();
        }
        if data.frame_id.is_empty() {
            data.frame_id = new_frame_id();
        }
        Self {
            transport,
            jar,
            username: username.to_string(),
            password: password.to_string(),
            data,
            cookie_path,
        }
    }

    pub fn session(&self) -> &SessionData {
        &self.data
    }

    pub fn session_mut(&mut self) -> &mut SessionData {
        &mut self.data
    }

    /// The setup endpoint for the account's partition.
    ///
    /// Apple shards accounts across partitions; talking to the wrong one
    /// answers with `421`. The partition is detected once and then reused.
    pub fn setup_endpoint(&self) -> String {
        match &self.data.partition {
            Some(partition) if !partition.is_empty() => {
                format!("https://p{}-setup.icloud.com/setup/ws/1", partition)
            }
            _ => SETUP_ENDPOINT.to_string(),
        }
    }

    /// Detect the account partition.
    pub fn detect_partition(&mut self) -> Result<String> {
        let request =
            HttpRequest::new(Method::Post, "https://setup.icloud.com/setup/ws/1/validate")
                .header("Origin", REDIRECT_URI)
                .header("Referer", format!("{}/", REDIRECT_URI))
                .header("Content-Type", "application/json")
                .json(json!({}));
        let response = self.transport.execute(&request)?;
        let partition = response
            .header("x-apple-user-partition")
            .ok_or_else(|| {
                Error::Protocol("the service did not report an account partition".into())
            })?
            .to_string();
        self.data.partition = Some(partition.clone());
        self.persist()?;
        Ok(partition)
    }

    fn frame_tag(&self) -> String {
        format!("auth-{}", self.data.frame_id)
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        let frame = self.frame_tag();
        let mut headers = vec![
            ("Accept".to_string(), "application/json".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Origin".to_string(), REDIRECT_URI.to_string()),
            ("Referer".to_string(), format!("{}/", REDIRECT_URI)),
            ("X-Apple-Widget-Key".to_string(), CLIENT_ID.to_string()),
            ("X-Apple-OAuth-Client-Id".to_string(), CLIENT_ID.to_string()),
            (
                "X-Apple-OAuth-Client-Type".to_string(),
                "firstPartyAuth".to_string(),
            ),
            (
                "X-Apple-OAuth-Redirect-URI".to_string(),
                REDIRECT_URI.to_string(),
            ),
            (
                "X-Apple-OAuth-Require-Grant-Code".to_string(),
                "true".to_string(),
            ),
            (
                "X-Apple-OAuth-Response-Mode".to_string(),
                "web_message".to_string(),
            ),
            (
                "X-Apple-OAuth-Response-Type".to_string(),
                "code".to_string(),
            ),
            ("X-Apple-OAuth-State".to_string(), frame.clone()),
            ("X-Apple-Frame-Id".to_string(), frame),
            ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
            (
                "X-Apple-Mandate-Security-Upgrade".to_string(),
                "0".to_string(),
            ),
            ("X-Apple-I-Require-UE".to_string(), "true".to_string()),
            (
                "X-Apple-I-FD-Client-Info".to_string(),
                format!(
                    "{{\"U\":\"{}\",\"L\":\"en-US\",\"Z\":\"GMT-05:00\",\"V\":\"1.1\",\"F\":\"\"}}",
                    USER_AGENT
                ),
            ),
        ];
        if let Some(session_id) = &self.data.session_id {
            headers.push(("X-Apple-ID-Session-Id".to_string(), session_id.clone()));
        }
        if let Some(scnt) = &self.data.scnt {
            headers.push(("scnt".to_string(), scnt.clone()));
        }
        headers
    }

    fn apply_headers(&self, request: HttpRequest) -> HttpRequest {
        let mut request = request;
        for (key, value) in self.auth_headers() {
            request = request.header(&key, value);
        }
        request
    }

    /// Capture the headers the service uses to carry session state.
    fn absorb(&mut self, response: &HttpResponse) {
        if let Some(value) = response.header("x-apple-id-session-id") {
            self.data.session_id = Some(value.to_string());
        }
        if let Some(value) = response.header("x-apple-session-token") {
            self.data.session_token = Some(value.to_string());
        }
        if let Some(value) = response.header("x-apple-twosv-trust-token") {
            self.data.trust_token = Some(value.to_string());
        }
        if let Some(value) = response.header("x-apple-id-account-country") {
            self.data.account_country = Some(value.to_string());
        }
        if let Some(value) = response.header("scnt") {
            self.data.scnt = Some(value.to_string());
        }
    }

    /// Start an authentication frame.
    fn auth_start(&mut self) -> Result<()> {
        let frame = self.frame_tag();
        let request = HttpRequest::new(Method::Get, format!("{}/authorize/signin", AUTH_ENDPOINT))
            .query("frame_id", frame.clone())
            .query("language", "en_US")
            .query("skVersion", "7")
            .query("iframeId", frame.clone())
            .query("client_id", CLIENT_ID)
            .query("redirect_uri", REDIRECT_URI)
            .query("response_type", "code")
            .query("response_mode", "web_message")
            .query("state", frame)
            .query("authVersion", "latest")
            .header("Accept", "*/*")
            .header("User-Agent", USER_AGENT);

        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            return Err(Error::AuthRequired);
        }
        Ok(())
    }

    /// Submit the account name.
    fn auth_federate(&mut self) -> Result<()> {
        let frame = self.frame_tag();
        let request = self
            .apply_headers(
                HttpRequest::new(Method::Post, format!("{}/federate", AUTH_ENDPOINT))
                    .query("isRememberMeEnabled", "true")
                    .query("frame_id", frame)
                    .json(json!({
                        "accountName": self.username,
                        "rememberMe": true,
                    })),
            )
            .header("Origin", REDIRECT_URI)
            .header("Referer", format!("{}/", REDIRECT_URI));

        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            return Err(Error::InvalidCredentials);
        }
        Ok(())
    }

    /// Sign in with the Apple ID.
    ///
    /// The password is never sent: it is used to answer the SRP challenge.
    pub fn signin(&mut self) -> Result<SignInOutcome> {
        self.auth_start()?;
        self.auth_federate()?;

        // Start the SRP exchange.
        let mut client = SrpClient::new()?;
        let request = self.apply_headers(
            HttpRequest::new(Method::Post, format!("{}/signin/init", AUTH_ENDPOINT)).json(json!({
                "a": base64::engine::general_purpose::STANDARD.encode(client.public_bytes()),
                "accountName": self.username,
                "protocols": ["s2k", "s2k_fo"],
            })),
        );
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            return Err(Error::InvalidCredentials);
        }
        let challenge = response.json()?;
        let salt = decode_base64(
            challenge
                .get("salt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Protocol("the challenge has no salt".into()))?,
        )?;
        let server_b = decode_base64(
            challenge
                .get("b")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Protocol("the challenge has no server value".into()))?,
        )?;
        let iterations = challenge
            .get("iteration")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as u32;
        let protocol = challenge
            .get("protocol")
            .and_then(|v| v.as_str())
            .unwrap_or("s2k")
            .to_string();
        let c = challenge
            .get("c")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        client.process_challenge(
            &self.username,
            &self.password,
            &salt,
            &server_b,
            iterations,
            &protocol,
        )?;

        // Complete the exchange.
        let trust_tokens: Vec<Value> = self
            .data
            .trust_token
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| json!({ "trustToken": t }))
            .collect();
        let request = self.apply_headers(
            HttpRequest::new(Method::Post, format!("{}/signin/complete", AUTH_ENDPOINT))
                .query("isRememberMeEnabled", "true")
                .json(json!({
                    "accountName": self.username,
                    "m1": base64::engine::general_purpose::STANDARD.encode(client.proof()),
                    "m2": base64::engine::general_purpose::STANDARD.encode(client.server_proof()),
                    "c": c,
                    "rememberMe": true,
                    "trustTokens": trust_tokens,
                })),
        );
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        self.persist()?;

        match response.status {
            200..=299 => {
                self.trust_session()?;
                self.account_login()?;
                Ok(SignInOutcome::Authenticated)
            }
            409 => {
                let phones = self.trusted_phone_numbers().unwrap_or_default();
                Ok(SignInOutcome::TwoFactorRequired {
                    trusted_phone_numbers: phones,
                })
            }
            401 | 403 => Err(Error::InvalidCredentials),
            412 => Err(Error::AuthRequired),
            450 => Err(Error::TermsNotAccepted),
            status => Err(Error::Api {
                status,
                code: error_code(&response),
                message: error_message(&response),
            }),
        }
    }

    /// Ask the service to push a code to the trusted devices.
    ///
    /// Recent iOS versions do not push the notification on their own, so the
    /// request has to be explicit.
    pub fn request_push(&mut self) -> Result<()> {
        let request = self.apply_headers(HttpRequest::new(
            Method::Put,
            format!("{}/verify/trusteddevice/securitycode", AUTH_ENDPOINT),
        ));
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 && response.status != 409 {
            return Err(Error::Api {
                status: response.status,
                code: error_code(&response),
                message: error_message(&response),
            });
        }
        Ok(())
    }

    /// The trusted phone numbers of the account.
    pub fn trusted_phone_numbers(&self) -> Result<Vec<TrustedPhone>> {
        let request = self.apply_headers(HttpRequest::new(
            Method::Get,
            format!("{}/verify/phone", AUTH_ENDPOINT),
        ));
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Ok(Vec::new());
        }
        let value = response.json()?;
        let mut phones = Vec::new();
        if let Some(items) = value.as_array() {
            for item in items {
                phones.push(TrustedPhone {
                    id: item.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                    push_mode: item
                        .get("pushMode")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    non_fteu: item.get("nonFTEU").and_then(|v| v.as_bool()),
                });
            }
        }
        Ok(phones)
    }

    /// Ask the service to send a code by SMS.
    pub fn request_sms_code(&mut self, phone_id: i64) -> Result<()> {
        let request = self.apply_headers(
            HttpRequest::new(Method::Put, format!("{}/verify/phone", AUTH_ENDPOINT)).json(json!({
                "phoneNumber": { "id": phone_id },
                "mode": "sms",
            })),
        );
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: error_code(&response),
                message: error_message(&response),
            });
        }
        Ok(())
    }

    /// Submit the six digit code of a push notification.
    pub fn submit_code(&mut self, code: &str) -> Result<()> {
        let request = self.apply_headers(
            HttpRequest::new(
                Method::Post,
                format!("{}/verify/trusteddevice/securitycode", AUTH_ENDPOINT),
            )
            .json(json!({ "securityCode": { "code": code } })),
        );
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            let code = error_code(&response).unwrap_or_default();
            if code == "-21669" {
                return Err(Error::InvalidCode);
            }
            return Err(Error::Api {
                status: response.status,
                code: Some(code),
                message: error_message(&response),
            });
        }
        self.persist()?;
        self.trust_session()?;
        self.account_login()?;
        Ok(())
    }

    /// Submit the six digit code that was sent by SMS.
    pub fn submit_sms_code(&mut self, code: &str, phone_id: i64) -> Result<()> {
        let request = self.apply_headers(
            HttpRequest::new(
                Method::Post,
                format!("{}/verify/phone/securitycode", AUTH_ENDPOINT),
            )
            .json(json!({
                "securityCode": { "code": code },
                "phoneNumber": { "id": phone_id },
                "mode": "sms",
            })),
        );
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            let code = error_code(&response).unwrap_or_default();
            if code == "-21669" {
                return Err(Error::InvalidCode);
            }
            return Err(Error::Api {
                status: response.status,
                code: Some(code),
                message: error_message(&response),
            });
        }
        self.persist()?;
        self.trust_session()?;
        self.account_login()?;
        Ok(())
    }

    /// Turn the session into a trusted one.
    pub fn trust_session(&mut self) -> Result<()> {
        let mut request = self.apply_headers(HttpRequest::new(
            Method::Get,
            format!("{}/2sv/trust", AUTH_ENDPOINT),
        ));
        if let Some(token) = &self.data.trust_token {
            request = request.header("X-Apple-TwoSV-Trust-Token", token.clone());
        }
        let response = self.transport.execute(&request)?;
        self.absorb(&response);
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: error_code(&response),
                message: error_message(&response),
            });
        }
        self.persist()?;
        Ok(())
    }

    /// Exchange the session token for the account description.
    pub fn account_login(&mut self) -> Result<Value> {
        let body = json!({
            "accountCountryCode": self.data.account_country.clone().unwrap_or_default(),
            "dsWebAuthToken": self.data.session_token.clone().unwrap_or_default(),
            "extended_login": true,
            "trustToken": self.data.trust_token.clone().unwrap_or_default(),
        });
        let request = HttpRequest::new(
            Method::Post,
            format!("{}/accountLogin", self.setup_endpoint()),
        )
        .header("Origin", REDIRECT_URI)
        .header("Referer", format!("{}/", REDIRECT_URI))
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(body);
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::AuthRequired);
        }
        let value = response.json()?;
        self.absorb(&response);
        self.absorb_webservices(&value);
        self.persist()?;
        Ok(value)
    }

    /// Check whether the stored session is still valid.
    pub fn validate(&mut self) -> Result<Value> {
        let request = HttpRequest::new(Method::Post, format!("{}/validate", self.setup_endpoint()))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .text("null");
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::AuthRequired);
        }
        let value = response.json()?;
        self.absorb(&response);
        self.absorb_webservices(&value);
        self.persist()?;
        Ok(value)
    }

    fn absorb_webservices(&mut self, account: &Value) {
        let Some(services) = account.get("webservices").and_then(|v| v.as_object()) else {
            return;
        };
        if let Some(url) = services
            .get("drivews")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
        {
            self.data.webservices.drivews = Some(url.to_string());
        }
        if let Some(url) = services
            .get("docws")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
        {
            self.data.webservices.docws = Some(url.to_string());
        }
    }

    /// Persist the session next to the cookies.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = &self.cookie_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let value = serde_json::json!({
            "session": self.data,
            "cookies": self
                .jar
                .lock()
                .map(|jar| jar.to_json())
                .unwrap_or(Value::Null),
        });
        std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
        restrict(path)?;
        Ok(())
    }

    /// Load a persisted session.
    pub fn load(cookie_dir: &Path, account: &str) -> Result<(SessionData, CookieJar)> {
        let path = session_path(cookie_dir, account);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let value: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        let data: SessionData =
            serde_json::from_value(value.get("session").cloned().unwrap_or(Value::Null))
                .unwrap_or_default();
        let jar = value
            .get("cookies")
            .map(CookieJar::from_json)
            .unwrap_or_default();
        Ok((data, jar))
    }
}

/// The file the session is persisted to.
pub fn session_path(cookie_dir: &Path, account: &str) -> PathBuf {
    let safe: String = account
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();
    cookie_dir.join(format!("{}.session.json", safe))
}

fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn decode_base64(value: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| Error::Protocol(format!("the service sent an invalid value: {}", e)))
}

pub(crate) fn error_code(response: &HttpResponse) -> Option<String> {
    let value: Value = serde_json::from_slice(&response.body).ok()?;
    value
        .get("errorCode")
        .or_else(|| value.get("serverErrorCode"))
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
}

pub(crate) fn error_message(response: &HttpResponse) -> String {
    let value: Value = match serde_json::from_slice(&response.body) {
        Ok(v) => v,
        Err(_) => return format!("HTTP {}", response.status),
    };
    value
        .get("errorMessage")
        .or_else(|| value.get("reason"))
        .or_else(|| value.get("errorReason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("HTTP {}", response.status))
}

/// Generate a client identifier for a session.
pub fn new_client_id() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut bytes)
        })
        .is_err()
    {
        return format!(
            "auth-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    hex::encode(bytes)
}

/// Generate the frame identifier of an authentication attempt.
pub fn new_frame_id() -> String {
    new_client_id().trim_start_matches("auth-").to_string()
}
