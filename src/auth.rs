use crate::{Error, Result, secret, text, transport};
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    hash::{MessageDigest, hash},
    pkey::PKey,
    rsa::Padding,
};
use percent_encoding::percent_decode_str;
use reqwest::{
    Client, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION, REFERER},
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use transport::{APP, IDENTITY, PORTAL};

const CALLBACK: &str = "/sso/callback/userauth";
struct Endpoints {
    identity: String,
    portal: String,
    app: String,
}

impl Endpoints {
    fn official() -> Self {
        Self {
            identity: IDENTITY.into(),
            portal: PORTAL.into(),
            app: APP.into(),
        }
    }

    fn approved(&self, url: &Url) -> bool {
        transport::approved(
            url,
            &[
                &self.identity,
                &self.portal,
                &self.app,
                "https://bjjj.jtgl.beijing.gov.cn",
            ],
        )
    }
}

/// Personal account information. Handle it as private data.
#[derive(Deserialize)]
pub struct Account {
    #[serde(rename = "userId", deserialize_with = "account_id")]
    pub id: String,
    #[serde(rename = "nickName", default)]
    pub display_name: Option<String>,
    #[serde(rename = "phonenumber", default)]
    pub phone_number: Option<String>,
}

fn account_id<'de, D: Deserializer<'de>>(de: D) -> std::result::Result<String, D::Error> {
    let value = Value::deserialize(de)?;
    let id = match value {
        Value::String(s) => s,
        Value::Number(n) if n.is_u64() => n.to_string(),
        _ => return Err(serde::de::Error::custom("invalid account id")),
    };
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|c| c.is_ascii_digit()) {
        return Err(serde::de::Error::custom("invalid account id"));
    }
    Ok(id)
}

/// An authorized Beijing Traffic Police App session held in memory.
pub struct AppSession {
    pub(crate) token: HeaderValue,
}

impl AppSession {
    /// Use a session token provided by an authorized integration.
    pub fn from_token(token: &str) -> Result<Self> {
        Ok(Self {
            token: secret(token, "app_token")?,
        })
    }

    /// Verify the session and return the current account.
    pub async fn account(&self) -> Result<Account> {
        self.account_with(&transport::client(false)?, APP).await
    }

    async fn account_with(&self, client: &Client, app: &str) -> Result<Account> {
        let request = client
            .get(format!("{app}/system/user/getInfo"))
            .header(AUTHORIZATION, self.token.clone());
        let response = transport::business(transport::json(request).await?, "account")?;
        serde_json::from_value(response["user"].clone()).map_err(|_| Error::InvalidResponse)
    }
}

/// Show this image to the account holder and ask them for the answer.
pub struct Captcha {
    pub mime_type: &'static str,
    pub bytes: Vec<u8>,
}

/// A single interactive natural-person login. Each instance owns a separate cookie store.
pub struct Login {
    http: Client,
    endpoints: Endpoints,
    page_url: Url,
    public_key: String,
    sms_mobile: Option<String>,
    sms_outcome_unknown: bool,
}

impl Login {
    /// Start the Beijing Traffic Police login flow through Beijing Unified Identity Authentication.
    /// The callback address belongs to Beijing Traffic Police.
    pub async fn start(client_id: &str, state: &str) -> Result<Self> {
        Self::start_with(
            transport::client(true)?,
            Endpoints::official(),
            client_id,
            state,
        )
        .await
    }

    async fn start_with(
        http: Client,
        endpoints: Endpoints,
        client_id: &str,
        state: &str,
    ) -> Result<Self> {
        text(client_id, "client_id", 128)?;
        text(state, "login_state", 256)?;
        let entry = http
            .get(format!(
                "{}/renzheng/open/m/login/goUserLogin",
                endpoints.identity
            ))
            .query(&[
                ("client_id", client_id),
                ("redirect_uri", &format!("{}{CALLBACK}", endpoints.app)),
                ("response_type", "code"),
                ("scope", "user_info"),
                ("state", state),
            ])
            .build()
            .map_err(|_| Error::InvalidInput("login_parameters"))?;
        let entry_url = entry.url().clone();
        let response = http.execute(entry).await.map_err(|_| Error::Transport)?;
        let page_url = redirect(&entry_url, &response)?;
        if !transport::approved(&page_url, &[&endpoints.identity, &endpoints.portal]) {
            return Err(Error::OfficialClientRequired);
        }
        let public_key = parameter(&page_url, "pubKey").ok_or(Error::InvalidResponse)?;
        if public_key.is_empty() || public_key.len() > 2048 {
            return Err(Error::InvalidResponse);
        }
        let response = http
            .get(page_url.clone())
            .header(REFERER, entry_url.as_str())
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() != StatusCode::OK {
            return Err(Error::OfficialClientRequired);
        }
        let login = Self {
            http,
            endpoints,
            page_url,
            public_key,
            sms_mobile: None,
            sms_outcome_unknown: false,
        };
        login.check_policy().await?;
        Ok(login)
    }

    /// Fetch a new human-readable challenge in this same login session.
    pub async fn captcha(&self) -> Result<Captcha> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Transport)?
            .as_millis();
        let response = self
            .http
            .get(format!(
                "{}/renzheng/common/generateCaptcha?{timestamp}",
                self.endpoints.identity
            ))
            .header(REFERER, self.page_url.as_str())
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() != StatusCode::OK
            || !response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("image/"))
        {
            return Err(Error::InvalidResponse);
        }
        let bytes = transport::bytes(response, 200_000).await?;
        let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else {
            return Err(Error::InvalidResponse);
        };
        Ok(Captcha { mime_type, bytes })
    }

    /// Send one SMS to the account holder after they solve the CAPTCHA.
    pub async fn send_sms(&mut self, mobile: &str, captcha: &str) -> Result<()> {
        if mobile.len() != 11 || !mobile.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput("mobile"));
        }
        text(captcha, "captcha", 32)?;
        if self.sms_outcome_unknown || self.sms_mobile.is_some() {
            return Err(Error::InvalidInput("sms_already_requested"));
        }
        self.check_policy().await?;
        self.sms_outcome_unknown = true;
        let reply = self
            .form(
                "inner/m/sendSmsBeforeLogin",
                &[
                    ("mobile", mobile),
                    ("captcha", captcha),
                    ("type", "login"),
                    ("smsType", "login_type"),
                ],
            )
            .await
            .and_then(|value| auth_code(&value, "send_sms"))
            .map_err(|error| transport::mutation(error, "send_sms"));
        match &reply {
            Ok(()) => {
                self.sms_mobile = Some(mobile.to_owned());
                self.sms_outcome_unknown = false;
            }
            Err(Error::Rejected { .. } | Error::Unauthorized) => self.sms_outcome_unknown = false,
            Err(_) => {}
        }
        reply
    }

    /// Complete SMS login with the code received by the previously selected account holder.
    pub async fn login_with_sms(&self, code: &str) -> Result<AppSession> {
        let mobile = self
            .sms_mobile
            .as_deref()
            .ok_or(Error::InvalidInput("sms_not_sent"))?;
        text(code, "sms_code", 32)?;
        self.check_policy().await?;
        let payload = encrypt(
            &self.public_key,
            &json!({"mobile": mobile, "smsCode": code}),
        )?;
        let value = self
            .form(
                "inner/m/login/doUserLoginBySms",
                &[("encryptData", &payload)],
            )
            .await
            .map_err(|error| transport::mutation(error, "login_with_sms"))?;
        self.finish(value).await
    }

    /// Complete password login with the account holder's CAPTCHA answer.
    pub async fn login_with_password(
        &self,
        username: &str,
        password: &str,
        captcha: &str,
    ) -> Result<AppSession> {
        text(username, "username", 128)?;
        text(password, "password", 256)?;
        text(captcha, "captcha", 32)?;
        self.check_policy().await?;
        let payload = encrypt(&self.public_key, &password_fields(username, password)?)?;
        let value = self
            .form(
                "inner/m/login/doUserLoginByPwd",
                &[("encryptData", &payload), ("captcha", captcha)],
            )
            .await
            .map_err(|error| transport::mutation(error, "login_with_password"))?;
        self.finish(value).await
    }

    async fn form(&self, path: &str, fields: &[(&str, &str)]) -> Result<Value> {
        transport::json(
            self.http
                .post(format!("{}/renzheng/{path}", self.endpoints.identity))
                .header(REFERER, self.page_url.as_str())
                .header("origin", &self.endpoints.portal)
                .header("x-requested-with", "XMLHttpRequest")
                .header("accept", "application/json")
                .form(fields),
        )
        .await
    }

    async fn check_policy(&self) -> Result<()> {
        let value = self.form("inner/m/login/checkH5LoginPermit", &[]).await?;
        auth_code(&value, "check_login_policy")?;
        if value["data"]["allowed"] == false {
            return Err(Error::OfficialClientRequired);
        }
        Ok(())
    }

    async fn finish(&self, value: Value) -> Result<AppSession> {
        auth_code(&value, "identity_login")?;
        let data = &value["data"];
        if level(&data["certLevel"])? < level(&data["grantLevel"])? {
            return Err(Error::OfficialClientRequired);
        }
        let next = data["redirectUrl"].as_str().ok_or(Error::InvalidResponse)?;
        let mut url = self
            .page_url
            .join(next)
            .map_err(|_| Error::InvalidResponse)?;
        for _ in 0..8 {
            if !self.endpoints.approved(&url) {
                return Err(Error::OfficialClientRequired);
            }
            if transport::approved(&url, &[&self.endpoints.app])
                && url.path() == CALLBACK
                && let Some(token) = parameter(&url, "token")
            {
                let session = exchange(&self.http, &self.endpoints, &url, &token).await?;
                session
                    .account_with(&self.http, &self.endpoints.app)
                    .await?;
                return Ok(session);
            }
            let response = self
                .http
                .get(url.clone())
                .header(REFERER, self.page_url.as_str())
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            url = redirect(&url, &response)?;
        }
        Err(Error::OfficialClientRequired)
    }
}

fn redirect(from: &Url, response: &reqwest::Response) -> Result<Url> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Err(Error::OfficialClientRequired);
    }
    response
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|location| from.join(location).ok())
        .ok_or(Error::OfficialClientRequired)
}

fn parameter(url: &Url, key: &str) -> Option<String> {
    // Some versions send unescaped '+' in the RSA public key.
    [url.query(), url.fragment()]
        .into_iter()
        .flatten()
        .flat_map(|part| part.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(name, value)| {
            if name != key {
                return None;
            }
            percent_decode_str(value)
                .decode_utf8()
                .ok()
                .map(|s| s.into_owned())
        })
}

fn auth_code(value: &Value, operation: &'static str) -> Result<()> {
    match transport::code(&value["meta"]["code"])? {
        0 => Ok(()),
        code => Err(Error::Rejected { operation, code }),
    }
}

fn level(value: &Value) -> Result<u8> {
    match value.as_str() {
        Some("L1" | "1") => Ok(1),
        Some("L2" | "2") => Ok(2),
        Some("L3" | "3") => Ok(3),
        Some("L31") => Ok(4),
        Some("L32") => Ok(5),
        Some("L33") => Ok(6),
        _ => match value.as_u64() {
            Some(n @ 1..=3) => Ok(n as u8),
            _ => Err(Error::InvalidResponse),
        },
    }
}

async fn exchange(
    http: &Client,
    endpoints: &Endpoints,
    url: &Url,
    token: &str,
) -> Result<AppSession> {
    if !(20..=256).contains(&token.len())
        || !token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return Err(Error::InvalidResponse);
    }
    let user_type = parameter(url, "userType")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Z".into());
    if user_type != "Z" {
        return Err(Error::OfficialClientRequired);
    }
    let redirect_url = parameter(url, "redirectUrl").unwrap_or_default();
    if !redirect_url.is_empty()
        && !Url::parse(&redirect_url).is_ok_and(|url| endpoints.approved(&url))
    {
        return Err(Error::OfficialClientRequired);
    }
    let reply = transport::json(
        http.get(format!("{}/auth/appLogin", endpoints.app))
            .query(&[
                ("token", token),
                ("userType", user_type.as_str()),
                ("redirectUrl", redirect_url.as_str()),
            ]),
    )
    .await?;
    let reply = transport::business(reply, "app_login")?;
    AppSession::from_token(
        reply["data"]["access_token"]
            .as_str()
            .ok_or(Error::InvalidResponse)?,
    )
    .map_err(|_| Error::InvalidResponse)
}

fn password_fields(username: &str, password: &str) -> Result<Value> {
    let digest = hash(MessageDigest::md5(), password.as_bytes()).map_err(|_| Error::Transport)?;
    let encrypted_pwd: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(json!({"userIdentity": username, "encryptedPwd": encrypted_pwd, "resetFlag": false}))
}

fn encrypt(public_key: &str, payload: &Value) -> Result<String> {
    let der = STANDARD
        .decode(public_key)
        .map_err(|_| Error::InvalidResponse)?;
    let key = PKey::public_key_from_der(&der)
        .and_then(|key| key.rsa())
        .map_err(|_| Error::InvalidResponse)?;
    let size = key.size() as usize;
    if !(128..=512).contains(&size) {
        return Err(Error::InvalidResponse);
    }
    let raw = serde_json::to_string(payload).map_err(|_| Error::InvalidInput("login_payload"))?;
    let mut encrypted = Vec::new();
    let mut block = vec![0; size];
    let mut start = 0;
    while start < raw.len() {
        let mut end = (start + size - 11).min(raw.len());
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        let count = key
            .public_encrypt(&raw.as_bytes()[start..end], &mut block, Padding::PKCS1)
            .map_err(|_| Error::InvalidResponse)?;
        encrypted.extend_from_slice(&block[..count]);
        start = end;
    }
    Ok(STANDARD.encode(encrypted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::{Json, response::IntoResponse};
    use openssl::rsa::Rsa;
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn password_and_sms_preserve_session_and_use_official_payloads() {
        let rsa = Rsa::generate(1024).unwrap();
        let public = STANDARD.encode(rsa.public_key_to_der().unwrap());
        let sms_requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&sms_requests);
        let origin = serve(move |method, uri, headers, bytes| {
            if uri.path().starts_with("/renzheng/") && !uri.path().ends_with("/goUserLogin") {
                assert_eq!(headers["cookie"], "session=local-synthetic");
            }
            match uri.path() {
                "/renzheng/open/m/login/goUserLogin" => {
                    assert_eq!(method, "GET");
                    assert!(uri.query().unwrap().contains("client_id=synthetic-client"));
                    (StatusCode::FOUND, [
                        ("location", format!("/renzheng/login?pubKey={public}")),
                        ("set-cookie", "session=local-synthetic; Path=/renzheng/".into())
                    ]).into_response()
                }
                "/renzheng/login" => StatusCode::OK.into_response(),
                "/renzheng/inner/m/login/checkH5LoginPermit" => {
                    Json(json!({"meta":{"code":0},"data":{"allowed":true}})).into_response()
                }
                "/renzheng/common/generateCaptcha" => {
                    ([("content-type", "image/png")], &b"\x89PNG\r\n\x1a\nfixture"[..]).into_response()
                }
                "/renzheng/inner/m/sendSmsBeforeLogin" => {
                    let payload = Url::parse(&format!("http://localhost/?{}", String::from_utf8_lossy(&bytes))).unwrap();
                    let form: BTreeMap<_, _> = payload.query_pairs().collect();
                    assert_eq!(form.get("captcha").unwrap(), "demo");
                    assert_eq!(form.get("mobile").unwrap(), "13000000000");
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"meta":{"code":0}})).into_response()
                }
                path @ ("/renzheng/inner/m/login/doUserLoginByPwd" | "/renzheng/inner/m/login/doUserLoginBySms") => {
                    assert_eq!(method, "POST");
                    let payload = Url::parse(&format!("http://localhost/?{}", String::from_utf8_lossy(&bytes))).unwrap();
                    let form: BTreeMap<_, _> = payload.query_pairs().collect();
                    let cipher = STANDARD.decode(form.get("encryptData").unwrap().as_bytes()).unwrap();
                    let mut clear = vec![0; rsa.size() as usize];
                    let n = rsa.private_decrypt(&cipher, &mut clear, Padding::PKCS1).unwrap();
                    let plain: Value = serde_json::from_slice(&clear[..n]).unwrap();
                    if path.ends_with("ByPwd") {
                        assert_eq!(form.get("captcha").unwrap(), "demo");
                        assert_eq!(plain, json!({"userIdentity":"synthetic","encryptedPwd":"5f4dcc3b5aa765d61d8327deb882cf99","resetFlag":false}));
                    } else {
                        assert_eq!(plain, json!({"mobile":"13000000000", "smsCode":"123456"}));
                    }
                    Json(json!({"meta":{"code":0},"data":{"certLevel":"L3","grantLevel":"L2",
                        "redirectUrl":"/sso/callback/userauth?token=synthetic-sso-callback-token"}})).into_response()
                }
                "/auth/appLogin" => {
                    assert!(uri.query().unwrap().contains("token=synthetic-sso-callback-token"));
                    Json(json!({"code":200,"data":{"access_token":"synthetic-app-token"}})).into_response()
                }
                "/system/user/getInfo" => {
                    assert_eq!(headers["authorization"], "synthetic-app-token");
                    Json(json!({"code":200,"user":{"userId":"90001"}})).into_response()
                }
                _ => panic!("unexpected path {}", uri.path()),
            }
        }).await;
        let start = || {
            Login::start_with(
                Client::builder()
                    .cookie_store(true)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                Endpoints {
                    identity: origin.clone(),
                    portal: origin.clone(),
                    app: origin.clone(),
                },
                "synthetic-client",
                "test-state",
            )
        };
        let login = start().await.unwrap();
        assert_eq!(login.captcha().await.unwrap().mime_type, "image/png");
        let app = login
            .login_with_password("synthetic", "password", "demo")
            .await
            .unwrap();
        assert_eq!(app.token, "synthetic-app-token");
        let mut login = start().await.unwrap();
        login.captcha().await.unwrap();
        login.send_sms("13000000000", "demo").await.unwrap();
        assert!(login.send_sms("13000000000", "demo").await.is_err());
        assert_eq!(sms_requests.load(Ordering::SeqCst), 1);
        let app = login.login_with_sms("123456").await.unwrap();
        assert_eq!(app.token, "synthetic-app-token");

        assert!(matches!(login.finish(json!({"meta":{"code":0},"data":{
            "certLevel":"L2","grantLevel":"L3","redirectUrl":"/sso/callback/userauth?token=ignored"
        }})).await, Err(Error::OfficialClientRequired)));
        assert!(matches!(login.finish(json!({"meta":{"code":0},"data":{
            "certLevel":"L3","grantLevel":"L2","redirectUrl":"https://example.invalid/sso/callback/userauth?token=ignored"
        }})).await, Err(Error::OfficialClientRequired)));
    }
}
