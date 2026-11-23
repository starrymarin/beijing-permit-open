use crate::{Error, Result};
use reqwest::{Client, RequestBuilder, StatusCode, Url, redirect::Policy};
use serde_json::Value;
use std::time::Duration;

pub(crate) const APP: &str = "https://ssp.jtgl.beijing.gov.cn";
pub(crate) const PERMIT: &str = "https://jjz.jtgl.beijing.gov.cn:2443";
pub(crate) const IDENTITY: &str = "https://bjt.beijing.gov.cn";
pub(crate) const PORTAL: &str = "https://portal.bjt.beijing.gov.cn";
const LIMIT: usize = 2 * 1024 * 1024;

pub(crate) fn client(cookies: bool) -> Result<Client> {
    Client::builder()
        .https_only(true)
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .cookie_store(cookies)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("beijing-permit-open/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Error::Transport)
}

pub(crate) async fn bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Error::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if chunk.len() > limit - body.len() {
            return Err(Error::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn json(request: RequestBuilder) -> Result<Value> {
    let response = request.send().await.map_err(|_| Error::Transport)?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(Error::Unauthorized),
        status => return Err(Error::Http(status.as_u16())),
    }
    let parsed: Value = serde_json::from_slice(&bytes(response, LIMIT).await?)
        .map_err(|_| Error::InvalidResponse)?;
    if !parsed.is_object() {
        return Err(Error::InvalidResponse);
    }
    Ok(parsed)
}

pub(crate) fn code(value: &Value) -> Result<u16> {
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or(Error::InvalidResponse)
}

pub(crate) fn business(value: Value, operation: &'static str) -> Result<Value> {
    match code(&value["code"])? {
        200 => Ok(value),
        401 | 403 => Err(Error::Unauthorized),
        code => Err(Error::Rejected { operation, code }),
    }
}

pub(crate) fn mutation(error: Error, operation: &'static str) -> Error {
    match error {
        Error::Rejected { code: 500.., .. }
        | Error::Transport
        | Error::Http(_)
        | Error::ResponseTooLarge
        | Error::InvalidResponse => Error::OutcomeUnknown(operation),
        other => other,
    }
}

pub(crate) fn approved(url: &Url, origins: &[&str]) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && origins.contains(&url.origin().ascii_serialization().as_str())
}
