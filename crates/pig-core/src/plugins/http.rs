use futures_util::StreamExt as _;
use pig_protocol::{
    AppConfig, CoreError, PluginHttpCredential, PluginHttpRequest, PluginHttpResponse,
};

#[cfg(test)]
use super::PluginRegistry;

const MAX_BODY: usize = 1024 * 1024;

pub async fn request_http(
    plugins: &super::SharedPluginRegistry,
    config: Option<&AppConfig>,
    package: &str,
    digest: &str,
    request: PluginHttpRequest,
    credentials: std::sync::Arc<crate::credentials::CredentialManager>,
) -> Result<PluginHttpResponse, CoreError> {
    let PluginHttpRequest {
        origin: origin_id,
        method,
        query,
        headers,
        mut body,
        credential,
    } = request;
    let (origin, credential_declaration) = {
        let registry = plugins.lock().expect("plugin registry lock");
        let origin = registry
            .http_origin(package, digest, &origin_id, &method, config)?
            .clone();
        let declaration = credential
            .as_ref()
            .map(|credential| {
                registry.credential_for_origin(
                    package,
                    digest,
                    &origin_id,
                    &method,
                    &credential.credential,
                    config,
                )
            })
            .transpose()?;
        (origin, declaration)
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pig-code-plugin-http/1")
        .build()
        .map_err(|error| transport(error.to_string()))?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|error| transport(error.to_string()))?;
    let mut builder = client.request(method.clone(), &origin.url).query(&query);
    for (name, value) in headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
        ) {
            continue;
        }
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| transport(error.to_string()))?;
        let value = reqwest::header::HeaderValue::from_str(&value)
            .map_err(|error| transport(error.to_string()))?;
        builder = builder.header(name, value);
    }
    let mut used_secret = None;
    if let Some(credential) = credential {
        let declaration = credential_declaration.expect("declared HTTP credential");
        let id = match declaration {
            pig_protocol::PluginCredentialDeclaration::Owned { .. } => {
                crate::credentials::CredentialId::plugin(package, declaration.id())
            }
            pig_protocol::PluginCredentialDeclaration::Provider { provider_id, .. } => {
                crate::credentials::CredentialId::provider(&provider_id)
            }
        };
        let secret = credentials
            .get(id.clone())
            .await
            .map_err(|error| error.into_core(&id))?;
        builder = inject_credential(builder, credential, &mut body, &secret)?;
        used_secret = Some(secret);
    }
    if let Some(body) = body {
        builder = builder.json(&body);
    }
    let response = builder
        .send()
        .await
        .map_err(|error| redact_error(transport(error.to_string()), used_secret.as_deref()))?;
    let status = response.status().as_u16();
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| redact_error(transport(error.to_string()), used_secret.as_deref()))?;
        if body.len() + chunk.len() > MAX_BODY {
            return Err(transport("plugin HTTP response exceeds 1 MiB".to_owned()));
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    Ok(PluginHttpResponse {
        status,
        body: used_secret
            .as_deref()
            .map_or(body.clone(), |secret| redact_secret(&body, secret)),
    })
}

fn inject_credential(
    mut builder: reqwest::RequestBuilder,
    credential: PluginHttpCredential,
    body: &mut Option<serde_json::Value>,
    secret: &str,
) -> Result<reqwest::RequestBuilder, CoreError> {
    let prefix = credential.prefix.unwrap_or_default();
    match (credential.header, credential.json_pointer) {
        (Some(name), None) => {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| transport(error.to_string()))?;
            let value = reqwest::header::HeaderValue::from_str(&format!("{prefix}{secret}"))
                .map_err(|error| transport(error.to_string()))?;
            builder = builder.header(name, value);
        }
        (None, Some(pointer)) => {
            let body = body.as_mut().ok_or_else(|| {
                transport("a JSON credential injection requires a request body".to_owned())
            })?;
            insert_json(
                body,
                &pointer,
                serde_json::Value::String(format!("{prefix}{secret}")),
            )?;
        }
        _ => {
            return Err(transport(
                "a credential must select exactly one header or JSON pointer".to_owned(),
            ));
        }
    }
    Ok(builder)
}

fn redact_secret(text: &str, secret: &str) -> String {
    text.replace(secret, super::REDACTED)
}

fn redact_error(error: CoreError, secret: Option<&str>) -> CoreError {
    let Some(secret) = secret else {
        return error;
    };
    match error {
        CoreError::PluginTransport { detail } => CoreError::PluginTransport {
            detail: redact_secret(&detail, secret),
        },
        CoreError::PluginPermission { detail } => CoreError::PluginPermission {
            detail: redact_secret(&detail, secret),
        },
        CoreError::CredentialStore { credential, detail } => CoreError::CredentialStore {
            credential,
            detail: redact_secret(&detail, secret),
        },
        other => other,
    }
}

fn insert_json(
    value: &mut serde_json::Value,
    pointer: &str,
    secret: serde_json::Value,
) -> Result<(), CoreError> {
    let mut segments: Vec<_> = pointer.split('/').skip(1).collect();
    let Some(last) = segments.pop() else {
        return Err(transport(
            "credential JSON pointer must name a field".to_owned(),
        ));
    };
    let mut target = value;
    for segment in segments {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        if !target.is_object() {
            *target = serde_json::json!({});
        }
        target = target
            .as_object_mut()
            .expect("created object")
            .entry(segment)
            .or_insert(serde_json::Value::Null);
    }
    if !target.is_object() {
        *target = serde_json::json!({});
    }
    target
        .as_object_mut()
        .expect("created object")
        .insert(last.replace("~1", "/").replace("~0", "~"), secret);
    Ok(())
}

fn transport(detail: impl Into<String>) -> CoreError {
    CoreError::PluginTransport {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{PluginPackageSource, PluginSource};

    #[tokio::test]
    async fn json_credentials_reach_the_request_but_are_redacted_from_the_response() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("pig-plugin-http-{}-{unique}", std::process::id()));
        let package = root.join("package");
        let data = root.join("data");
        std::fs::create_dir_all(&package).expect("package directory");
        std::fs::write(package.join("backend.ts"), "export {};").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind HTTP mock");
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("HTTP request");
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).expect("request headers");
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            std::io::Read::read_exact(&mut reader, &mut body).expect("request body");
            let body = String::from_utf8(body).expect("JSON body");
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\nConnection: close\r\n\r\necho json-test-secret";
            std::io::Write::write_all(&mut stream, response).expect("HTTP response");
            body
        });
        std::fs::write(
            package.join("pig-plugin.json"),
            format!(
                r#"{{
                  "apiVersion": 1,
                  "id": "demo",
                  "name": "Demo",
                  "version": "1.0.0",
                  "build": 1,
                  "minHostVersion": "1.0.0",
                  "protocols": {{"ui": 1, "services": 1}},
                  "capabilities": [],
                  "contributions": {{
                    "actions": ["http"],
                    "credentials": [{{
                      "kind": "owned",
                      "id": "test-credential",
                      "label": "Test credential"
                    }}],
                    "http": [{{
                      "id": "local",
                      "url": "http://{address}/search",
                      "methods": ["POST"],
                      "credentials": ["test-credential"]
                    }}]
                  }}
                }}"#
            ),
        )
        .unwrap();
        let mut registry = PluginRegistry::new(data.clone());
        let snapshot = registry.sync(vec![PluginPackageSource {
            root: package,
            source: PluginSource::User,
        }]);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        let package = &snapshot.packages[0];
        registry.set_enabled("demo", &package.digest, true).unwrap();
        let plugins = registry.into_shared();
        let credentials = crate::credentials::CredentialManager::empty_memory();
        credentials
            .set(
                crate::credentials::CredentialId::plugin("demo", "test-credential"),
                "json-test-secret".into(),
            )
            .await
            .expect("store credential");
        let response = request_http(
            &plugins,
            None,
            "demo",
            &package.digest,
            PluginHttpRequest {
                origin: "local".into(),
                method: "POST".into(),
                query: Default::default(),
                headers: Default::default(),
                body: Some(serde_json::json!({"query": "docs", "max_results": 5})),
                credential: Some(PluginHttpCredential {
                    credential: "test-credential".into(),
                    header: None,
                    json_pointer: Some("/auth/api_key".into()),
                    prefix: None,
                }),
            },
            std::sync::Arc::new(credentials),
        )
        .await
        .expect("HTTP response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "echo ${PIG_REDACTED}");
        let body: serde_json::Value =
            serde_json::from_str(&server.join().expect("HTTP server")).expect("request JSON");
        assert_eq!(body["auth"]["api_key"], "json-test-secret");
        assert_eq!(body["max_results"], 5);
        let _ = std::fs::remove_dir_all(root);
    }
}
