use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, instrument};

use super::{ExecResult, SandboxBackend, SandboxBackendConfig, SandboxCapabilities, SandboxError};

const ENVD_PORT: u16 = 49983;
const CONNECT_CONTENT_TYPE: &str = "application/connect+json";
const CONNECT_PROTOCOL_VERSION: &str = "1";
const CONNECT_END_STREAM_FLAG: u8 = 0x02;
const CONNECT_COMPRESSED_FLAG: u8 = 0x01;
const DEFAULT_ENVD_USER: &str = "root";

/// CubeSandbox-backed command execution.
#[derive(Debug, Clone)]
pub struct CubeSandboxBackend {
    config: SandboxBackendConfig,
    client: reqwest::Client,
}

impl CubeSandboxBackend {
    pub fn new(config: SandboxBackendConfig) -> Result<Self, SandboxError> {
        if config.template_id.is_none() {
            return Err(SandboxError::RequestFailed(
                "CubeSandbox template_id is required".to_string(),
            ));
        }

        let mut headers = HeaderMap::new();
        if let Some(api_key) = &config.api_key {
            let value = HeaderValue::from_str(&format!("Bearer {}", api_key))
                .map_err(|e| SandboxError::RequestFailed(format!("invalid api_key: {}", e)))?;
            headers.insert("authorization", value);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(config.timeout_seconds.max(30)))
            .build()?;

        Ok(Self { config, client })
    }

    fn api_url(&self) -> String {
        self.config.api_url.trim_end_matches('/').to_string()
    }

    fn envd_url(&self, sandbox_id: &str) -> String {
        if let Some(override_url) = &self.config.envd_override {
            let base = override_url.trim_end_matches('/');
            return format!("{}/process.Process/Start", base);
        }
        let host = if let Some(proxy_ip) = &self.config.proxy_node_ip {
            format!("{}:{}", proxy_ip, self.config.proxy_port)
        } else {
            format!("{}-{sandbox_id}.{}", ENVD_PORT, self.config.domain)
        };
        format!("http://{}/process.Process/Start", host)
    }

    async fn create_sandbox(&self) -> Result<CreatedSandbox, SandboxError> {
        let template_id = self
            .config
            .template_id
            .as_ref()
            .expect("template_id checked in constructor")
            .clone();

        let payload = json!({
            "templateID": template_id,
            "timeout": self.config.timeout_seconds,
        });

        debug!(url = %self.api_url(), %template_id, "creating CubeSandbox");

        let resp = self
            .client
            .post(format!("{}/sandboxes", self.api_url()))
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(SandboxError::ApiError {
                status: status.as_u16(),
                message,
            });
        }

        let data: serde_json::Value = resp.json().await?;
        let sandbox_id = data["sandboxID"]
            .as_str()
            .ok_or_else(|| {
                SandboxError::StreamError("missing sandboxID in create response".into())
            })?
            .to_string();
        let envd_access_token = data["envdAccessToken"].as_str().map(|s| s.to_string());
        let traffic_access_token = data["trafficAccessToken"].as_str().map(|s| s.to_string());

        Ok(CreatedSandbox {
            sandbox_id,
            envd_access_token,
            traffic_access_token,
        })
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> Result<(), SandboxError> {
        let url = format!("{}/sandboxes/{}", self.api_url(), sandbox_id);
        debug!(%url, "killing CubeSandbox");

        let resp = self.client.delete(&url).send().await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(());
        }
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(SandboxError::ApiError {
                status: status.as_u16(),
                message,
            });
        }
        Ok(())
    }

    fn encode_connect_envelope(data: &[u8], flags: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + data.len());
        out.push(flags);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    fn build_command_payload(
        command: &str,
        cwd: &Path,
        envs: &serde_json::Map<String, serde_json::Value>,
    ) -> serde_json::Value {
        let mut process = serde_json::Map::new();
        process.insert("cmd".into(), "/bin/bash".into());
        process.insert("args".into(), json!(["-l", "-c", command]));
        if !envs.is_empty() {
            process.insert("envs".into(), envs.clone().into());
        }
        process.insert("cwd".into(), cwd.to_string_lossy().to_string().into());

        json!({
            "process": process,
            "stdin": false,
        })
    }

    #[instrument(skip(self, created), fields(sandbox_id = %created.sandbox_id))]
    async fn run_command_in_sandbox(
        &self,
        created: &CreatedSandbox,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError> {
        let url = self.envd_url(&created.sandbox_id);
        debug!(%url, command, "running command in CubeSandbox");

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(CONNECT_CONTENT_TYPE));
        headers.insert(
            "Connect-Protocol-Version",
            HeaderValue::from_static(CONNECT_PROTOCOL_VERSION),
        );
        headers.insert(
            "Connect-Content-Encoding",
            HeaderValue::from_static("identity"),
        );
        headers.insert(
            "Connect-Timeout-Ms",
            HeaderValue::from_str(&(timeout_secs * 1000).to_string())
                .map_err(|e| SandboxError::RequestFailed(format!("invalid timeout: {}", e)))?,
        );
        let sandbox_id = &created.sandbox_id;
        if self.config.proxy_node_ip.is_some() {
            let host = format!("{}-{sandbox_id}.{}", ENVD_PORT, self.config.domain);
            headers.insert(
                "Host",
                HeaderValue::from_str(&host).map_err(|e| {
                    SandboxError::RequestFailed(format!("invalid envd host header: {}", e))
                })?,
            );
        }
        if let Some(token) = &created.envd_access_token {
            headers.insert(
                "X-Access-Token",
                HeaderValue::from_str(token).map_err(|e| {
                    SandboxError::RequestFailed(format!("invalid envd access token: {}", e))
                })?,
            );
        }
        if let Some(token) = &created.traffic_access_token {
            headers.insert(
                "e2b-traffic-access-token",
                HeaderValue::from_str(token).map_err(|e| {
                    SandboxError::RequestFailed(format!("invalid traffic access token: {}", e))
                })?,
            );
        }
        headers.insert("x-user", HeaderValue::from_static(DEFAULT_ENVD_USER));

        let payload = Self::build_command_payload(command, cwd, &serde_json::Map::new());
        let body = Self::encode_connect_envelope(payload.to_string().as_bytes(), 0);

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .body(body)
            .timeout(Duration::from_secs(timeout_secs.max(30)))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(SandboxError::ApiError {
                status: status.as_u16(),
                message,
            });
        }

        let bytes = resp.bytes().await?;
        Self::parse_connect_stream(&bytes)
    }

    fn parse_connect_stream(data: &[u8]) -> Result<ExecResult, SandboxError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code: Option<i32> = None;
        let mut buffer = data;

        while buffer.len() >= 5 {
            let flags = buffer[0];
            let size = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
            if buffer.len() < 5 + size {
                break;
            }

            let raw = &buffer[5..5 + size];
            buffer = &buffer[5 + size..];

            if flags & CONNECT_COMPRESSED_FLAG != 0 {
                return Err(SandboxError::StreamError(
                    "compressed envelope not supported".into(),
                ));
            }

            if flags & CONNECT_END_STREAM_FLAG != 0 {
                if raw.is_empty() {
                    continue;
                }
                let payload: serde_json::Value = serde_json::from_slice(raw)
                    .map_err(|e| SandboxError::StreamError(e.to_string()))?;
                if let Some(error) = payload.get("error") {
                    let message = error
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Connect stream error");
                    let code = error.get("code").and_then(|v| v.as_str());
                    return Err(SandboxError::StreamError(
                        code.map(|c| format!("{}: {}", c, message))
                            .unwrap_or_else(|| message.to_string()),
                    ));
                }
                continue;
            }

            let envelope: serde_json::Value = serde_json::from_slice(raw)
                .map_err(|e| SandboxError::StreamError(e.to_string()))?;
            let event = envelope.get("event").cloned().unwrap_or_default();
            let data = event.get("data").cloned().unwrap_or_default();

            if let Some(out) = data.get("stdout").and_then(|v| v.as_str()) {
                stdout.push(base64_decode(out)?);
            }
            if let Some(err) = data.get("stderr").and_then(|v| v.as_str()) {
                stderr.push(base64_decode(err)?);
            }

            if let Some(end) = event.get("end") {
                if let Some(code) = end.get("exitCode").and_then(|v| v.as_i64()) {
                    exit_code = Some(code as i32);
                } else if let Some(code) = end.get("exit_code").and_then(|v| v.as_i64()) {
                    exit_code = Some(code as i32);
                } else if let Some(status) = end.get("status") {
                    exit_code = exit_code_from_status(status);
                } else if end.get("error").is_some() {
                    return Err(SandboxError::StreamError(
                        end["error"]
                            .as_str()
                            .unwrap_or("process failed")
                            .to_string(),
                    ));
                }
            }
        }

        if !buffer.is_empty() {
            return Err(SandboxError::StreamError(
                "Connect stream ended with a partial message".into(),
            ));
        }

        Ok(ExecResult {
            exit_code: exit_code.unwrap_or(-1),
            stdout: stdout.concat(),
            stderr: stderr.concat(),
        })
    }
}

#[async_trait]
impl SandboxBackend for CubeSandboxBackend {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError> {
        let created = self.create_sandbox().await?;
        let result = self
            .run_command_in_sandbox(&created, command, cwd, timeout_secs)
            .await;
        // Best-effort cleanup; do not let cleanup failures mask command result.
        if let Err(e) = self.kill_sandbox(&created.sandbox_id).await {
            tracing::warn!(error = %e, sandbox_id = %created.sandbox_id, "failed to kill CubeSandbox");
        }
        result
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem_isolation: true,
            network_isolation: true,
            process_isolation: true,
            reusable: false,
        }
    }
}

#[derive(Debug, Clone)]
struct CreatedSandbox {
    sandbox_id: String,
    envd_access_token: Option<String>,
    traffic_access_token: Option<String>,
}

fn base64_decode(input: &str) -> Result<String, SandboxError> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, input)
        .map_err(|e| SandboxError::StreamError(format!("invalid base64: {}", e)))?;
    String::from_utf8(bytes).map_err(|e| SandboxError::StreamError(format!("invalid utf8: {}", e)))
}

fn exit_code_from_status(status: &serde_json::Value) -> Option<i32> {
    let s = status.as_str()?;
    if s == "exited" || s == "OK" {
        return Some(0);
    }
    if let Some(caps) = regex::Regex::new(r"(?:exit status|exited with code)\s+(-?\d+)")
        .ok()?
        .captures(s)
    {
        return caps.get(1)?.as_str().parse::<i32>().ok();
    }
    if let Some(caps) = regex::Regex::new(r"(?:signal|terminated by signal)\s+(\d+)")
        .ok()?
        .captures(s)
    {
        return caps
            .get(1)
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .map(|sig| 128 + sig);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    fn connect_event(event: serde_json::Value) -> Vec<u8> {
        let data = event.to_string().into_bytes();
        CubeSandboxBackend::encode_connect_envelope(&data, 0)
    }

    fn connect_end() -> Vec<u8> {
        CubeSandboxBackend::encode_connect_envelope(&[], CONNECT_END_STREAM_FLAG)
    }

    #[test]
    fn parse_connect_stream_decodes_output_and_exit_code() {
        let stdout = b64("hello out");
        let stderr = b64("hello err");
        let event1 = json!({
            "event": {
                "data": { "stdout": stdout, "stderr": stderr }
            }
        });
        let event2 = json!({
            "event": { "end": { "exitCode": 42 } }
        });

        let mut stream = Vec::new();
        stream.extend(connect_event(event1));
        stream.extend(connect_event(event2));
        stream.extend(connect_end());

        let result = CubeSandboxBackend::parse_connect_stream(&stream).unwrap();
        assert_eq!(result.exit_code, 42);
        assert_eq!(result.stdout, "hello out");
        assert_eq!(result.stderr, "hello err");
    }

    #[test]
    fn parse_connect_stream_end_stream_error() {
        let error_event = json!({
            "error": { "code": "internal", "message": "boom" }
        });
        let mut stream = Vec::new();
        stream.extend(connect_event(
            json!({ "event": { "end": { "exitCode": 0 } } }),
        ));
        stream.extend(CubeSandboxBackend::encode_connect_envelope(
            error_event.to_string().as_bytes(),
            CONNECT_END_STREAM_FLAG,
        ));

        let err = CubeSandboxBackend::parse_connect_stream(&stream).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn exit_code_from_status_parses_variants() {
        assert_eq!(exit_code_from_status(&json!("exited with code 7")), Some(7));
        assert_eq!(
            exit_code_from_status(&json!("terminated by signal 9")),
            Some(137)
        );
        assert_eq!(exit_code_from_status(&json!("exited")), Some(0));
        assert_eq!(exit_code_from_status(&json!("unknown status")), None);
    }

    #[tokio::test]
    async fn cube_backend_exec_full_lifecycle() {
        let server = MockServer::start().await;
        let sandbox_id = "sb-test-123";

        // 1. Create sandbox.
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/sandboxes"))
                    .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                        "sandboxID": sandbox_id,
                        "templateID": "tpl-code",
                        "envdAccessToken": "envd-token",
                        "trafficAccessToken": "traffic-token",
                        "domain": "cube.app",
                    }))),
            )
            .await;

        // 2. Run command via envd process API.
        let stdout = b64("sandbox stdout");
        let stderr = b64("sandbox stderr");
        let stream = {
            let mut s = Vec::new();
            s.extend(connect_event(json!({
                "event": { "data": { "stdout": stdout, "stderr": stderr } }
            })));
            s.extend(connect_event(json!({
                "event": { "end": { "exitCode": 0 } }
            })));
            s.extend(connect_end());
            s
        };
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/process.Process/Start"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("Content-Type", CONNECT_CONTENT_TYPE)
                            .set_body_bytes(stream),
                    ),
            )
            .await;

        // 3. Kill sandbox.
        server
            .register(
                Mock::given(method("DELETE"))
                    .and(path_regex(format!("/sandboxes/{}", sandbox_id)))
                    .respond_with(ResponseTemplate::new(204)),
            )
            .await;

        let backend = CubeSandboxBackend::new(SandboxBackendConfig {
            backend: "cube".to_string(),
            api_url: server.uri(),
            template_id: Some("tpl-code".to_string()),
            api_key: Some("cube-key".to_string()),
            domain: "cube.app".to_string(),
            proxy_node_ip: None,
            proxy_port: 80,
            envd_override: Some(format!("http://{}", server.address())),
            timeout_seconds: 60,
        })
        .unwrap();

        let result = backend
            .exec("echo hello", Path::new("/workspace"), 30)
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "sandbox stdout");
        assert_eq!(result.stderr, "sandbox stderr");
    }

    #[tokio::test]
    async fn cube_backend_create_error_surfaces_api_error() {
        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/sandboxes"))
                    .respond_with(
                        ResponseTemplate::new(500)
                            .set_body_json(json!({ "message": "cluster down" })),
                    ),
            )
            .await;

        let backend = CubeSandboxBackend::new(SandboxBackendConfig {
            backend: "cube".to_string(),
            api_url: server.uri(),
            template_id: Some("tpl-code".to_string()),
            api_key: None,
            domain: "cube.app".to_string(),
            proxy_node_ip: None,
            proxy_port: 80,
            envd_override: None,
            timeout_seconds: 60,
        })
        .unwrap();

        let err = backend.exec("true", Path::new("/"), 10).await.unwrap_err();
        assert!(err.to_string().contains("cluster down"));
    }
}
