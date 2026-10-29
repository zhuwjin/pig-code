/// Normalized token usage for one provider response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
    pub used: u64,
    pub total: u64,
    pub reasoning_output: u64,
}

/// Provider-supplied response identifiers. HTTP request/trace IDs, CDN rays,
/// and the response body's object ID are distinct namespaces and are never
/// synthesized locally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderResponseMetadata {
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub response_id: Option<String>,
    pub cf_ray: Option<String>,
}

impl ProviderResponseMetadata {
    pub(crate) fn from_response(response: &reqwest::Response) -> Self {
        Self::from_headers(response.status(), response.headers())
    }

    fn from_headers(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            http_status: Some(status.as_u16()),
            request_id: header_value(headers, &["x-request-id", "request-id"]),
            trace_id: header_value(headers, &["x-trace-id", "x-cloud-trace-context"]),
            response_id: None,
            cf_ray: header_value(headers, &["cf-ray"]),
        }
    }
}

fn header_value(headers: &reqwest::header::HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    #[test]
    fn response_ids_use_explicit_header_fallbacks() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("request-id"),
            HeaderValue::from_static("request-fallback"),
        );
        headers.insert(
            HeaderName::from_static("x-cloud-trace-context"),
            HeaderValue::from_static("cloud-trace"),
        );
        headers.insert(
            HeaderName::from_static("cf-ray"),
            HeaderValue::from_static("cdn-ray"),
        );
        let metadata = ProviderResponseMetadata::from_headers(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &headers,
        );
        assert_eq!(metadata.http_status, Some(429));
        assert_eq!(metadata.request_id.as_deref(), Some("request-fallback"));
        assert_eq!(metadata.trace_id.as_deref(), Some("cloud-trace"));
        assert_eq!(metadata.cf_ray.as_deref(), Some("cdn-ray"));
        assert_eq!(metadata.response_id, None);
    }

    #[test]
    fn primary_headers_win_and_empty_values_are_missing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("primary-request"),
        );
        headers.insert(
            HeaderName::from_static("request-id"),
            HeaderValue::from_static("fallback-request"),
        );
        headers.insert(
            HeaderName::from_static("x-trace-id"),
            HeaderValue::from_static("   "),
        );
        let metadata = ProviderResponseMetadata::from_headers(reqwest::StatusCode::OK, &headers);
        assert_eq!(metadata.request_id.as_deref(), Some("primary-request"));
        assert_eq!(metadata.trace_id, None);
    }
}
