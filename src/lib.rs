//! Async client for authorized Beijing entry permit integrations.
//! See the README for the workflow, upstream field names and verification scope.
#![forbid(unsafe_code)]

mod auth;
mod permit;
mod transport;

pub use auth::{Account, AppSession, Captcha, Login};
pub use permit::{Application, Id, PermitClient, PermitKind, Vehicle, VehicleRegistration};

use reqwest::header::HeaderValue;

/// Errors intentionally omit credentials, request URLs and upstream response bodies.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input for {0}")]
    InvalidInput(&'static str),
    #[error("upstream transport failed")]
    Transport,
    #[error("upstream HTTP status {0}")]
    Http(u16),
    #[error("upstream response exceeded its size limit")]
    ResponseTooLarge,
    #[error("unexpected upstream response")]
    InvalidResponse,
    #[error("upstream authorization required")]
    Unauthorized,
    #[error("upstream rejected {operation} with code {code}")]
    Rejected { operation: &'static str, code: u16 },
    #[error("upstream requires completion in the official client")]
    OfficialClientRequired,
    #[error("outcome of {0} is unknown; reconcile before sending another request")]
    OutcomeUnknown(&'static str),
    #[error("application history could not be completely paginated")]
    Pagination,
}

pub type Result<T> = std::result::Result<T, Error>;

fn text<'a>(value: &'a str, field: &'static str, limit: usize) -> Result<&'a str> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(field));
    }
    Ok(value)
}

fn secret(value: &str, field: &'static str) -> Result<HeaderValue> {
    text(value, field, 8192)?;
    let mut header = HeaderValue::from_str(value).map_err(|_| Error::InvalidInput(field))?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod test_support {
    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, Method, Uri},
        response::Response,
    };
    use std::sync::Arc;

    pub async fn serve(
        respond: impl Fn(Method, Uri, HeaderMap, Bytes) -> Response + Send + Sync + 'static,
    ) -> String {
        let respond = Arc::new(respond);
        let router = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| async move {
                respond(method, uri, headers, body)
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }
}
