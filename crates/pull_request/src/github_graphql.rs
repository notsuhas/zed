//! Minimal GitHub GraphQL transport over Zed's [`HttpClient`].
//!
//! A single `POST https://api.github.com/graphql` with `{query, variables}`,
//! `Authorization: bearer <token>`. Parses the `{data, errors}` envelope and
//! surfaces GraphQL `errors` as `anyhow` failures so callers don't silently
//! deserialize a partial/empty `data`.

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

pub const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

#[derive(Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

/// Execute a GraphQL `query` with `variables`, deserializing the `data` field
/// into `T`. Returns an error if the HTTP request fails, if GraphQL `errors`
/// are present, or if `data` is missing.
pub async fn execute<T: DeserializeOwned>(
    http_client: &Arc<dyn HttpClient>,
    token: Option<&str>,
    query: &str,
    variables: serde_json::Value,
) -> Result<T> {
    let payload = serde_json::json!({ "query": query, "variables": variables });
    let body = serde_json::to_string(&payload).context("serializing GraphQL request")?;

    let mut builder = Request::post(GITHUB_GRAPHQL_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", "Zed");

    if let Some(token) = token {
        builder = builder.header("Authorization", format!("bearer {token}"));
    }

    let request = builder.body(AsyncBody::from(body))?;
    let mut response = http_client.send(request).await?;

    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!(
            "GitHub GraphQL HTTP {}: {}",
            response.status().as_u16(),
            text
        );
    }

    let envelope: GraphQlEnvelope<T> =
        serde_json::from_slice(&bytes).context("parsing GraphQL response envelope")?;

    if !envelope.errors.is_empty() {
        let messages = envelope
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        bail!("GitHub GraphQL error: {messages}");
    }

    envelope
        .data
        .context("GitHub GraphQL response had no data")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Viewer {
        login: String,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct ViewerData {
        viewer: Viewer,
    }

    #[test]
    fn parses_data_envelope() {
        let json = br#"{"data":{"viewer":{"login":"octocat"}}}"#;
        let envelope: GraphQlEnvelope<ViewerData> = serde_json::from_slice(json).unwrap();
        assert_eq!(envelope.data.unwrap().viewer.login, "octocat");
        assert!(envelope.errors.is_empty());
    }

    #[test]
    fn surfaces_errors() {
        let json = br#"{"data":null,"errors":[{"message":"Bad credentials"}]}"#;
        let envelope: GraphQlEnvelope<ViewerData> = serde_json::from_slice(json).unwrap();
        assert!(envelope.data.is_none());
        assert_eq!(envelope.errors.len(), 1);
        assert_eq!(envelope.errors[0].message, "Bad credentials");
    }
}
