use std::sync::Arc;

use cel_interpreter::Duration;
use serde::Serialize;
use url::Url;

use crate::AddContentLength;

use super::{MaybeUtf8, PduName, ProtocolName};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename = "http")]
pub struct HttpOutput {
    pub name: ProtocolName,
    pub plan: HttpPlanOutput,
    pub request: Option<Arc<HttpRequestOutput>>,
    pub response: Option<Arc<HttpResponse>>,
    pub errors: Vec<HttpError>,
    pub protocol: Option<String>,
    pub duration: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpPlanOutput {
    pub url: Url,
    pub method: Option<MaybeUtf8>,
    pub add_content_length: AddContentLength,
    pub headers: Vec<(MaybeUtf8, MaybeUtf8)>,
    pub body: MaybeUtf8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename = "http_request")]
pub struct HttpRequestOutput {
    pub name: PduName,
    pub url: Url,
    pub protocol: MaybeUtf8,
    pub method: Option<MaybeUtf8>,
    pub headers: Vec<(MaybeUtf8, MaybeUtf8)>,
    pub body: MaybeUtf8,
    pub duration: Duration,
    pub body_duration: Option<Duration>,
    pub time_to_first_byte: Option<Duration>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename = "http_response")]
pub struct HttpResponse {
    pub name: PduName,
    pub protocol: Option<MaybeUtf8>,
    pub status_code: Option<u16>,
    pub headers: Option<Vec<(MaybeUtf8, MaybeUtf8)>>,
    pub body: Option<MaybeUtf8>,
    pub duration: Duration,
    pub header_duration: Option<Duration>,
    pub time_to_first_byte: Option<Duration>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpError {
    pub kind: String,
    pub message: String,
}
