//! Extism guest plugin for Diaryx live sharing.
//!
//! `diaryx.share` owns the live-share UI and session orchestration. It prefers
//! reusing `diaryx.sync`'s runtime through `host_plugin_command`, but can fall
//! back to a temporary in-plugin CRDT session runtime when sync is unavailable.

#[cfg(not(target_arch = "wasm32"))]
mod native_extism_stubs;

use diaryx_plugin_sdk::prelude::*;
diaryx_plugin_sdk::register_getrandom_v02!();

use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use diaryx_core::frontmatter;
use diaryx_core::plugin::{ComponentRef, SettingsField, SidebarSide, UiContribution};
use diaryx_plugin_sdk::protocol::ServerFunctionDecl;
use diaryx_sync::{IncomingEvent, SessionAction};
use diaryx_sync_extism::{binary_protocol, state};
use extism_pdk::*;
use serde_json::Value as JsonValue;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

// ============================================================================
// HTTP compat helpers (adapt SDK's typed HttpResponse to old JsonValue API)
// ============================================================================

fn http_request_compat(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_json: Option<JsonValue>,
) -> Result<JsonValue, String> {
    let header_map: std::collections::HashMap<String, String> =
        headers.iter().cloned().collect();
    let body_str = body_json.map(|b| b.to_string());
    let resp = host::http::request(method, url, &header_map, body_str.as_deref())?;
    let mut result = serde_json::json!({
        "status": resp.status,
        "body": resp.body,
    });
    if let Some(b64) = &resp.body_base64 {
        result["body_base64"] = JsonValue::String(b64.clone());
    }
    Ok(result)
}

fn http_request_binary_compat(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<JsonValue, String> {
    let header_map: std::collections::HashMap<String, String> =
        headers.iter().cloned().collect();
    let resp = host::http::request_binary(method, url, &header_map, body)?;
    let mut result = serde_json::json!({
        "status": resp.status,
        "body": resp.body,
    });
    if let Some(b64) = &resp.body_base64 {
        result["body_base64"] = JsonValue::String(b64.clone());
    }
    Ok(result)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct InitParams {
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
struct ShareExtismConfig {
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    active_join_code: Option<String>,
    #[serde(default)]
    share_read_only: Option<bool>,
    #[serde(default)]
    share_role: Option<String>,
    #[serde(default)]
    session_workspace_id: Option<String>,
    #[serde(default)]
    created_session_workspace: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct RuntimeContext {
    #[serde(skip)]
    raw: JsonValue,
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    guest_mode: bool,
    #[serde(default)]
    current_workspace: Option<RuntimeWorkspace>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct RuntimeWorkspace {
    #[serde(default)]
    local_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    plugin_metadata: Option<std::collections::HashMap<String, JsonValue>>,
    #[serde(default)]
    provider_links: Vec<RuntimeProviderLink>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct RuntimeProviderLink {
    #[serde(default)]
    plugin_id: Option<String>,
    #[serde(default)]
    remote_workspace_id: Option<String>,
    #[serde(default)]
    sync_enabled: Option<bool>,
}

fn load_extism_config() -> ShareExtismConfig {
    match host::storage::get("share.extism.config") {
        Ok(Some(bytes)) => serde_json::from_slice::<ShareExtismConfig>(&bytes).unwrap_or_default(),
        _ => ShareExtismConfig::default(),
    }
}

fn save_extism_config(config: &ShareExtismConfig) {
    if let Ok(bytes) = serde_json::to_vec(config) {
        let _ = host::storage::set("share.extism.config", &bytes);
    }
}

fn normalize_server_base(server_url: &str) -> String {
    let mut base = server_url.trim().trim_end_matches('/').to_string();
    loop {
        if let Some(stripped) = base.strip_suffix("/sync2") {
            base = stripped.trim_end_matches('/').to_string();
            continue;
        }
        if let Some(stripped) = base.strip_suffix("/sync") {
            base = stripped.trim_end_matches('/').to_string();
            continue;
        }
        break;
    }
    base
}

fn command_param_str(params: &JsonValue, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn command_param_bool(params: &JsonValue, key: &str) -> Option<bool> {
    params.get(key).and_then(|v| v.as_bool())
}

fn apply_config_patch(config: &mut ShareExtismConfig, incoming: &JsonValue) {
    apply_config_string(config, incoming, "server_url", |cfg, value| cfg.server_url = value);
    apply_config_string(config, incoming, "auth_token", |cfg, value| cfg.auth_token = value);
    apply_config_string(config, incoming, "active_join_code", |cfg, value| {
        cfg.active_join_code = value.map(|s| s.to_uppercase())
    });
    apply_config_bool(config, incoming, "share_read_only", |cfg, value| {
        cfg.share_read_only = value
    });
    apply_config_string(config, incoming, "share_role", |cfg, value| cfg.share_role = value);
    apply_config_string(config, incoming, "session_workspace_id", |cfg, value| {
        cfg.session_workspace_id = value
    });
}

fn apply_config_string<F>(config: &mut ShareExtismConfig, incoming: &JsonValue, key: &str, set: F)
where
    F: FnOnce(&mut ShareExtismConfig, Option<String>),
{
    if let Some(raw) = incoming.get(key) {
        if raw.is_null() {
            set(config, None);
        } else if let Some(value) = raw.as_str() {
            let normalized = value.trim();
            if normalized.is_empty() {
                set(config, None);
            } else {
                set(config, Some(normalized.to_string()));
            }
        }
    }
}

fn apply_config_bool<F>(config: &mut ShareExtismConfig, incoming: &JsonValue, key: &str, set: F)
where
    F: FnOnce(&mut ShareExtismConfig, Option<bool>),
{
    if let Some(raw) = incoming.get(key) {
        if raw.is_null() {
            set(config, None);
        } else if let Some(value) = raw.as_bool() {
            set(config, Some(value));
        }
    }
}

fn load_runtime_context() -> RuntimeContext {
    let raw = host::context::get().unwrap_or_else(|_| serde_json::json!({}));
    let mut runtime = serde_json::from_value::<RuntimeContext>(raw.clone()).unwrap_or_default();
    runtime.raw = raw;
    runtime
}

fn resolve_server_url(
    params: &JsonValue,
    config: &ShareExtismConfig,
    runtime: &RuntimeContext,
) -> Result<String, String> {
    trim_option(command_param_str(params, "server_url"))
        .or_else(|| {
            runtime
                .has_field("server_url")
                .then(|| trim_option(runtime.server_url.clone()))
                .flatten()
        })
        .or_else(|| {
            (!runtime.has_field("server_url"))
                .then(|| trim_option(config.server_url.clone()))
                .flatten()
        })
        .or_else(|| {
            (!runtime.has_field("server_url"))
                .then(|| trim_option(runtime.server_url.clone()))
                .flatten()
        })
        .map(|value| normalize_server_base(&value))
        .ok_or("Missing server_url".to_string())
}

fn resolve_auth_token(
    params: &JsonValue,
    config: &ShareExtismConfig,
    runtime: &RuntimeContext,
) -> Option<String> {
    trim_option(command_param_str(params, "auth_token"))
        .or_else(|| {
            runtime
                .has_field("auth_token")
                .then(|| trim_option(runtime.auth_token.clone()))
                .flatten()
        })
        .or_else(|| {
            (!runtime.has_field("auth_token"))
                .then(|| trim_option(config.auth_token.clone()))
                .flatten()
        })
        .or_else(|| {
            (!runtime.has_field("auth_token"))
                .then(|| trim_option(runtime.auth_token.clone()))
                .flatten()
        })
}

fn runtime_workspace_name(runtime: &RuntimeContext) -> String {
    runtime
        .current_workspace
        .as_ref()
        .and_then(|workspace| workspace.name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "My Workspace".to_string())
}

fn runtime_workspace_root(params: &JsonValue, runtime: &RuntimeContext) -> Option<String> {
    command_param_str(params, "workspace_root")
        .or_else(|| runtime.current_workspace.as_ref().and_then(|w| w.path.clone()))
}

fn current_workspace_root() -> Option<String> {
    load_runtime_context()
        .current_workspace
        .and_then(|workspace| workspace.path)
}

fn trim_option(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl RuntimeContext {
    fn has_field(&self, key: &str) -> bool {
        self.raw.get(key).is_some()
    }
}

fn runtime_sync_workspace_id(runtime: &RuntimeContext) -> Option<String> {
    let current_workspace = runtime.current_workspace.as_ref()?;

    for link in &current_workspace.provider_links {
        let plugin_id = link.plugin_id.as_deref().map(str::trim).unwrap_or_default();
        let remote_workspace_id = link
            .remote_workspace_id
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if plugin_id == "diaryx.sync" && !remote_workspace_id.is_empty() {
            return Some(remote_workspace_id.to_string());
        }
    }

    let metadata = current_workspace.plugin_metadata.as_ref()?;
    let sync_meta = metadata
        .get("diaryx.sync")
        .or_else(|| metadata.get("sync"))?;
    if let Some(remote_workspace_id) = sync_meta
        .get("remoteWorkspaceId")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(remote_workspace_id.to_string());
    }
    sync_meta
        .get("serverId")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn effective_config(config: &ShareExtismConfig, runtime: &RuntimeContext) -> ShareExtismConfig {
    ShareExtismConfig {
        server_url: resolve_server_url(&JsonValue::Null, config, runtime).ok(),
        auth_token: None,
        active_join_code: config.active_join_code.clone(),
        share_read_only: config.share_read_only,
        share_role: config.share_role.clone(),
        session_workspace_id: config.session_workspace_id.clone(),
        created_session_workspace: config.created_session_workspace,
    }
}

fn auth_headers(auth_token: Option<String>) -> Vec<(String, String)> {
    match auth_token {
        Some(token) if !token.trim().is_empty() => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {}", token)),
        ],
        _ => vec![("Content-Type".to_string(), "application/json".to_string())],
    }
}

fn parse_http_status(response: &JsonValue) -> u64 {
    response.get("status").and_then(|v| v.as_u64()).unwrap_or(0)
}

fn parse_http_body(response: &JsonValue) -> String {
    response
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn parse_http_body_json(response: &JsonValue) -> Option<JsonValue> {
    let body = parse_http_body(response);
    if body.is_empty() {
        return None;
    }
    serde_json::from_str(&body).ok()
}

fn parse_http_body_bytes(response: &JsonValue) -> Result<Vec<u8>, String> {
    if let Some(body_b64) = response.get("body_base64").and_then(|v| v.as_str()) {
        if body_b64.is_empty() {
            return Ok(Vec::new());
        }
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .map_err(|e| format!("Invalid HTTP response body_base64: {e}"));
    }
    Ok(parse_http_body(response).into_bytes())
}

fn http_error(status: u64, body: &str) -> String {
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    }
}

fn normalize_snapshot_entry_path(path: &str) -> Option<String> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        None
    } else {
        Some(normalized.to_string_lossy().replace('\\', "/"))
    }
}

fn should_skip_snapshot_entry(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        let Component::Normal(part) = component else {
            return false;
        };
        let part = part.to_string_lossy();
        part.starts_with('.')
            || part == "__MACOSX"
            || part == "Thumbs.db"
            || part == "desktop.ini"
            || part.starts_with("._")
    })
}

fn resolve_workspace_path(workspace_root: Option<&str>, relative_path: &str) -> String {
    let root = workspace_root.map(str::trim).unwrap_or_default();
    if root.is_empty() || root == "." {
        return relative_path.to_string();
    }
    let mut full_path = PathBuf::from(root);
    full_path.push(relative_path);
    full_path.to_string_lossy().replace('\\', "/")
}

fn ensure_parent_dirs_for_binary(path: &str) -> Result<(), String> {
    let Some(parent) = Path::new(path).parent() else {
        return Ok(());
    };
    let parent_str = parent.to_string_lossy();
    if parent_str.is_empty() || parent_str == "." {
        return Ok(());
    }
    let marker_path = format!(
        "{}/.diaryx_share_tmp_parent",
        parent_str.trim_end_matches('/').trim_end_matches('\\')
    );
    host::fs::write_file(&marker_path, "")?;
    let _ = host::fs::delete_file(&marker_path);
    Ok(())
}

fn relative_snapshot_path(workspace_root: Option<&str>, path: &str) -> Option<String> {
    let mut candidate = path.replace('\\', "/");
    if let Some(root) = workspace_root {
        let normalized_root = root
            .trim()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        if !normalized_root.is_empty() && normalized_root != "." {
            if candidate == normalized_root {
                return None;
            }
            if let Some(stripped) = candidate.strip_prefix(&(normalized_root.clone() + "/")) {
                candidate = stripped.to_string();
            }
        }
    }
    let candidate = candidate
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    if candidate.is_empty() {
        return None;
    }
    normalize_snapshot_entry_path(&candidate)
}

fn build_workspace_snapshot_zip(
    workspace_root: Option<&str>,
    include_attachments: bool,
) -> Result<(Vec<u8>, usize), String> {
    let prefix = workspace_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .unwrap_or(".");
    let mut files = host::fs::list_files(prefix)?;
    files.sort();

    let cursor = Cursor::new(Vec::<u8>::new());
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut files_added = 0usize;

    for file_path in files {
        let Some(relative_path) = relative_snapshot_path(workspace_root, &file_path) else {
            continue;
        };
        if should_skip_snapshot_entry(&relative_path) {
            continue;
        }

        if relative_path.ends_with(".md") {
            let content = host::fs::read_file(&file_path)?;
            zip.start_file(relative_path, options)
                .map_err(|e| format!("Failed to add markdown entry to zip: {e}"))?;
            zip.write_all(content.as_bytes())
                .map_err(|e| format!("Failed to write markdown entry to zip: {e}"))?;
            files_added += 1;
            continue;
        }

        if include_attachments {
            let bytes = host::fs::read_binary(&file_path)?;
            zip.start_file(relative_path, options)
                .map_err(|e| format!("Failed to add binary entry to zip: {e}"))?;
            zip.write_all(&bytes)
                .map_err(|e| format!("Failed to write binary entry to zip: {e}"))?;
            files_added += 1;
        }
    }

    let cursor = zip
        .finish()
        .map_err(|e| format!("Failed to finalize snapshot zip: {e}"))?;
    Ok((cursor.into_inner(), files_added))
}

fn upload_workspace_snapshot(
    server: &str,
    auth_token: Option<String>,
    remote_id: &str,
    workspace_root: Option<&str>,
    mode: &str,
    include_attachments: bool,
) -> Result<usize, String> {
    let mut headers = auth_headers(auth_token);
    headers.push(("Content-Type".to_string(), "application/zip".to_string()));

    let (snapshot_zip, files_added) =
        build_workspace_snapshot_zip(workspace_root, include_attachments)?;
    let response = http_request_binary_compat(
        "POST",
        &format!(
            "{server}/api/workspaces/{remote_id}/snapshot?mode={mode}&include_attachments={include_attachments}"
        ),
        &headers,
        &snapshot_zip,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }

    Ok(files_added)
}

fn download_workspace_snapshot(
    server: &str,
    auth_token: Option<String>,
    remote_id: &str,
    workspace_root: Option<&str>,
    include_attachments: bool,
) -> Result<usize, String> {
    let headers = auth_headers(auth_token);
    let response = http_request_compat(
        "GET",
        &format!(
            "{server}/api/workspaces/{remote_id}/snapshot?include_attachments={include_attachments}"
        ),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }

    let snapshot_bytes = parse_http_body_bytes(&response)?;
    if snapshot_bytes.is_empty() {
        return Err("Snapshot download returned empty body".to_string());
    }

    let mut archive = ZipArchive::new(Cursor::new(snapshot_bytes))
        .map_err(|e| format!("Invalid snapshot zip: {e}"))?;
    let mut files_imported = 0usize;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("Failed to read zip entry #{index}: {e}"))?;
        if entry.is_dir() {
            continue;
        }

        let raw_name = entry.name().to_string();
        if should_skip_snapshot_entry(&raw_name) {
            continue;
        }
        let Some(relative_path) = normalize_snapshot_entry_path(&raw_name) else {
            continue;
        };

        let target_path = resolve_workspace_path(workspace_root, &relative_path);
        if relative_path.ends_with(".md") {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| format!("Failed to read markdown entry {relative_path}: {e}"))?;
            host::fs::write_file(&target_path, &content)?;
        } else {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Failed to read binary entry {relative_path}: {e}"))?;
            ensure_parent_dirs_for_binary(&target_path)?;
            host::fs::write_binary(&target_path, &bytes)?;
        }
        files_imported += 1;
    }

    Ok(files_imported)
}

fn create_remote_workspace(
    server: &str,
    auth_token: Option<String>,
    name: &str,
) -> Result<String, String> {
    let response = http_request_compat(
        "POST",
        &format!("{server}/api/workspaces"),
        &auth_headers(auth_token),
        Some(serde_json::json!({ "name": name })),
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    let body = parse_http_body_json(&response).ok_or("Invalid workspace creation response")?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or("Missing id in workspace creation response".to_string())
}

fn delete_remote_workspace(
    server: &str,
    auth_token: Option<String>,
    remote_id: &str,
) -> Result<(), String> {
    let response = http_request_compat(
        "DELETE",
        &format!("{server}/api/workspaces/{remote_id}"),
        &auth_headers(auth_token),
        None,
    )?;
    let status = parse_http_status(&response);
    if status == 200 || status == 204 {
        Ok(())
    } else {
        Err(http_error(status, &parse_http_body(&response)))
    }
}

fn lookup_share_session(
    server: &str,
    join_code: &str,
) -> Result<(String, bool), String> {
    let response = http_request_compat(
        "GET",
        &format!("{server}/sessions/{}", join_code.to_uppercase()),
        &[("Content-Type".to_string(), "application/json".to_string())],
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    let body = parse_http_body_json(&response).ok_or("Invalid session response")?;
    let namespace_id = body
        .get("namespace_id")
        .and_then(|value| value.as_str())
        .ok_or("Missing namespace_id in session response")?
        .to_string();
    let read_only = body
        .get("read_only")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    Ok((namespace_id, read_only))
}

fn create_share_session(
    server: &str,
    auth_token: Option<String>,
    namespace_id: &str,
    read_only: bool,
) -> Result<(String, bool), String> {
    let response = http_request_compat(
        "POST",
        &format!("{server}/sessions"),
        &auth_headers(auth_token),
        Some(serde_json::json!({
            "namespace_id": namespace_id,
            "read_only": read_only,
        })),
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    let body = parse_http_body_json(&response).ok_or("Invalid session response")?;
    let join_code = body
        .get("code")
        .and_then(|value| value.as_str())
        .ok_or("Missing code in session response")?
        .to_string();
    let read_only = body
        .get("read_only")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    Ok((join_code, read_only))
}

fn delete_share_session(
    server: &str,
    auth_token: Option<String>,
    join_code: &str,
) -> Result<(), String> {
    let response = http_request_compat(
        "DELETE",
        &format!("{server}/sessions/{}", join_code.to_uppercase()),
        &auth_headers(auth_token),
        None,
    )?;
    let status = parse_http_status(&response);
    if status == 200 || status == 204 {
        Ok(())
    } else {
        Err(http_error(status, &parse_http_body(&response)))
    }
}

fn update_share_read_only(
    server: &str,
    auth_token: Option<String>,
    join_code: &str,
    read_only: bool,
) -> Result<(), String> {
    let response = http_request_compat(
        "PATCH",
        &format!("{server}/sessions/{}", join_code.to_uppercase()),
        &auth_headers(auth_token),
        Some(serde_json::json!({ "read_only": read_only })),
    )?;
    let status = parse_http_status(&response);
    if status == 200 {
        Ok(())
    } else {
        Err(http_error(status, &parse_http_body(&response)))
    }
}

fn ensure_fallback_session(workspace_id: &str, write_to_disk: bool) -> Result<(), String> {
    state::create_session(workspace_id, write_to_disk).map_err(|e| e.to_string())
}

fn connect_session_runtime(
    server: &str,
    workspace_id: &str,
    auth_token: Option<&str>,
    session_code: &str,
    write_to_disk: bool,
) -> Result<bool, String> {
    let prepare_params = serde_json::json!({
        "workspace_id": workspace_id,
        "write_to_disk": write_to_disk,
    });
    if host::plugins::call("diaryx.sync", "PrepareLiveShareRuntime", prepare_params).is_ok()
    {
        host::plugins::call(
            "diaryx.sync",
            "ConnectLiveShareSession",
            serde_json::json!({
                "server_url": server,
                "workspace_id": workspace_id,
                "auth_token": auth_token,
                "session_code": session_code,
                "write_to_disk": write_to_disk,
            }),
        )?;
        return Ok(true);
    }

    ensure_fallback_session(workspace_id, write_to_disk)?;
    host::ws::connect(
        server,
        workspace_id,
        auth_token,
        Some(session_code),
        Some(write_to_disk),
    )?;
    Ok(false)
}

fn disconnect_session_runtime() -> Result<(), String> {
    if host::plugins::call(
        "diaryx.sync",
        "DisconnectLiveShareSession",
        serde_json::json!({}),
    )
    .is_ok()
    {
        return Ok(());
    }
    host::ws::disconnect()
}

fn execute_session_actions(actions: Vec<SessionAction>, workspace_root: Option<&str>) {
    let runtime = load_runtime_context();
    let config = load_extism_config();
    let server = resolve_server_url(&JsonValue::Null, &config, &runtime).ok();
    let auth_token = resolve_auth_token(&JsonValue::Null, &config, &runtime);

    let mut queue: VecDeque<SessionAction> = actions.into();
    loop {
        while let Some(action) = queue.pop_front() {
            match action {
                SessionAction::SendBinary(data) => {
                    if let Err(e) = host::ws::send_binary(&data) {
                        host::log::log("warn", &format!("[share:send_binary] {e}"));
                    }
                }
                SessionAction::SendText(text) => {
                    if let Err(e) = host::ws::send_text(&text) {
                        host::log::log("warn", &format!("[share:send_text] {e}"));
                    }
                }
                SessionAction::Emit(event) => state::emit_sync_event(&event),
                SessionAction::DownloadSnapshot { workspace_id } => {
                    if let Some(server) = server.as_deref() {
                        let follow_up = download_workspace_snapshot(
                            server,
                            auth_token.clone(),
                            &workspace_id,
                            workspace_root,
                            true,
                        )
                        .map(|_| {
                            state::with_session_mut(|session| {
                                poll_future(session.process(IncomingEvent::SnapshotImported))
                            })
                        })
                        .unwrap_or_else(|e| {
                            host::log::log(
                                "warn",
                                &format!("[share:snapshot_download] {e}"),
                            );
                            Ok(None)
                        })
                        .unwrap_or_else(|e| {
                            host::log::log(
                                "warn",
                                &format!("[share:snapshot_imported] {e}"),
                            );
                            None
                        })
                        .unwrap_or_default();
                        queue.extend(follow_up);
                    }
                }
            }
        }

        let local_updates = state::drain_local_updates();
        if local_updates.is_empty() {
            break;
        }

        let follow_up = state::with_session_mut(|session| {
            let mut actions = Vec::new();
            for (doc_id, data) in local_updates {
                actions.extend(poll_future(session.process(IncomingEvent::LocalUpdate {
                    doc_id,
                    data,
                })));
            }
            actions
        })
        .unwrap_or_else(|e| {
            host::log::log("warn", &format!("[share:local_update] {e}"));
            None
        })
        .unwrap_or_default();
        queue.extend(follow_up);
    }
}

fn ensure_remote_workspace(
    params: &JsonValue,
    config: &mut ShareExtismConfig,
    runtime: &RuntimeContext,
    server: &str,
    auth_token: Option<String>,
) -> Result<(String, Option<JsonValue>), String> {
    if let Some(workspace_id) = config.session_workspace_id.clone() {
        return Ok((workspace_id, None));
    }

    if let Some(workspace_id) = runtime_sync_workspace_id(runtime) {
        config.session_workspace_id = Some(workspace_id.clone());
        save_extism_config(config);
        return Ok((workspace_id, None));
    }

    let workspace_id = create_remote_workspace(server, auth_token.clone(), &runtime_workspace_name(runtime))?;
    let workspace_root = runtime_workspace_root(params, runtime);
    let _ = upload_workspace_snapshot(
        server,
        auth_token,
        &workspace_id,
        workspace_root.as_deref(),
        "replace",
        true,
    )?;
    config.session_workspace_id = Some(workspace_id.clone());
    config.created_session_workspace = true;
    save_extism_config(config);

    Ok((
        workspace_id.clone(),
        Some(serde_json::json!({
            "workspace_metadata_patch": {
                    "plugin_id": "diaryx.sync",
                    "data": {
                        "remoteWorkspaceId": workspace_id,
                        "serverId": workspace_id,
                        "syncEnabled": false,
                    }
            }
        })),
    ))
}

fn handle_create_share_session(params: &JsonValue) -> Result<JsonValue, String> {
    let runtime = load_runtime_context();
    let mut config = load_extism_config();
    let server = resolve_server_url(params, &config, &runtime)?;
    let auth_token = resolve_auth_token(params, &config, &runtime);
    let read_only = command_param_bool(params, "read_only")
        .or(config.share_read_only)
        .unwrap_or(false);

    let (workspace_id, metadata_patch) =
        ensure_remote_workspace(params, &mut config, &runtime, &server, auth_token.clone())?;
    let (join_code, effective_read_only) =
        create_share_session(&server, auth_token.clone(), &workspace_id, read_only)?;

    let _ = connect_session_runtime(
        &server,
        &workspace_id,
        auth_token.as_deref(),
        &join_code,
        true,
    )?;

    config.active_join_code = Some(join_code.clone());
    config.share_read_only = Some(effective_read_only);
    config.share_role = Some("host".to_string());
    config.session_workspace_id = Some(workspace_id.clone());
    save_extism_config(&config);

    let mut response = serde_json::json!({
        "join_code": join_code.clone(),
        "workspace_id": workspace_id.clone(),
        "read_only": effective_read_only,
        "message": format!("Session created. Join code: {join_code}"),
        "config_patch": {
            "active_join_code": join_code,
            "share_read_only": effective_read_only,
            "share_role": "host",
            "session_workspace_id": workspace_id,
        }
    });

    if let Some(patch) = metadata_patch
        && let Some(object) = response.as_object_mut()
        && let Some(workspace_patch) = patch.get("workspace_metadata_patch")
    {
        object.insert(
            "workspace_metadata_patch".to_string(),
            workspace_patch.clone(),
        );
    }

    Ok(response)
}

fn handle_join_share_session(params: &JsonValue) -> Result<JsonValue, String> {
    let runtime = load_runtime_context();
    let config = load_extism_config();
    let server = resolve_server_url(params, &config, &runtime)?;
    let join_code = command_param_str(params, "join_code")
        .or_else(|| config.active_join_code.clone())
        .ok_or("Missing join_code".to_string())?
        .to_uppercase();
    let (workspace_id, read_only) = lookup_share_session(&server, &join_code)?;

    Ok(serde_json::json!({
        "host_action": {
            "type": "enter-guest-workspace",
            "payload": {
                "session_code": join_code,
            }
        },
        "follow_up": {
            "command": "FinalizeJoinShareSession",
            "params": {
                "join_code": join_code,
                "workspace_id": workspace_id,
                "read_only": read_only,
            }
        }
    }))
}

fn handle_finalize_join_share_session(params: &JsonValue) -> Result<JsonValue, String> {
    let runtime = load_runtime_context();
    let mut config = load_extism_config();
    let server = resolve_server_url(params, &config, &runtime)?;
    let auth_token = resolve_auth_token(params, &config, &runtime);
    let join_code = command_param_str(params, "join_code").ok_or("Missing join_code".to_string())?;
    let workspace_id =
        command_param_str(params, "workspace_id").ok_or("Missing workspace_id".to_string())?;
    let read_only = command_param_bool(params, "read_only").unwrap_or(false);

    let _ = connect_session_runtime(
        &server,
        &workspace_id,
        auth_token.as_deref(),
        &join_code,
        false,
    )?;

    config.active_join_code = Some(join_code.to_uppercase());
    config.share_read_only = Some(read_only);
    config.share_role = Some("guest".to_string());
    config.session_workspace_id = Some(workspace_id.clone());
    save_extism_config(&config);

    Ok(serde_json::json!({
        "join_code": join_code.to_uppercase(),
        "workspace_id": workspace_id,
        "read_only": read_only,
        "message": "Joined session.",
        "config_patch": {
            "active_join_code": join_code.to_uppercase(),
            "share_read_only": read_only,
            "share_role": "guest",
            "session_workspace_id": workspace_id,
        }
    }))
}

fn handle_end_share_session(params: &JsonValue) -> Result<JsonValue, String> {
    let runtime = load_runtime_context();
    let mut config = load_extism_config();
    let server = resolve_server_url(params, &config, &runtime).ok();
    let auth_token = resolve_auth_token(params, &config, &runtime);

    if config.share_role.as_deref() == Some("host")
        && let (Some(server), Some(join_code)) = (server.as_deref(), config.active_join_code.clone())
    {
        let _ = delete_share_session(server, auth_token.clone(), &join_code);
    }

    if config.created_session_workspace
        && let (Some(server), Some(workspace_id)) =
            (server.as_deref(), config.session_workspace_id.clone())
    {
        let _ = delete_remote_workspace(server, auth_token.clone(), &workspace_id);
    }

    let _ = disconnect_session_runtime();

    let leave_guest = config.share_role.as_deref() == Some("guest");
    let clear_sync_metadata = config.created_session_workspace;
    config.active_join_code = None;
    config.share_read_only = None;
    config.share_role = None;
    config.session_workspace_id = None;
    config.created_session_workspace = false;
    save_extism_config(&config);

    let mut response = serde_json::json!({
        "ok": true,
        "message": "Session ended.",
        "config_patch": {
            "active_join_code": JsonValue::Null,
            "share_read_only": JsonValue::Null,
            "share_role": JsonValue::Null,
            "session_workspace_id": JsonValue::Null,
        }
    });

    if leave_guest && let Some(object) = response.as_object_mut() {
        object.insert(
            "host_action".to_string(),
            serde_json::json!({
                "type": "leave-guest-workspace"
            }),
        );
    }

    if clear_sync_metadata && let Some(object) = response.as_object_mut() {
        object.insert(
            "workspace_metadata_patch".to_string(),
            serde_json::json!({
                "plugin_id": "diaryx.sync",
                "data": JsonValue::Null,
            }),
        );
    }

    Ok(response)
}

fn handle_set_share_read_only(params: &JsonValue) -> Result<JsonValue, String> {
    let runtime = load_runtime_context();
    let mut config = load_extism_config();
    let server = resolve_server_url(params, &config, &runtime)?;
    let auth_token = resolve_auth_token(params, &config, &runtime);
    let read_only = command_param_bool(params, "read_only")
        .or(config.share_read_only)
        .unwrap_or(false);
    let join_code = config
        .active_join_code
        .clone()
        .ok_or("No active share session".to_string())?;

    update_share_read_only(&server, auth_token, &join_code, read_only)?;
    config.share_read_only = Some(read_only);
    save_extism_config(&config);

    Ok(serde_json::json!({
        "read_only": read_only,
        "message": if read_only {
            "Session is now read-only."
        } else {
            "Session is editable."
        },
        "config_patch": {
            "share_read_only": read_only
        }
    }))
}

fn build_manifest() -> GuestManifest {
    let share_settings_tab = UiContribution::SettingsTab {
        id: "share-settings".into(),
        label: "Live Share".into(),
        icon: None,
        fields: vec![
            SettingsField::AuthStatus {
                label: "Account".into(),
                description: Some("Sign in to host live sharing sessions.".into()),
            },
            SettingsField::UpgradeBanner {
                feature: "Live Share".into(),
                description: Some("Upgrade to share workspaces in real time.".into()),
            },
        ],
        component: None,
    };

    let share_tab = UiContribution::SidebarTab {
        id: "share".into(),
        label: "Share".into(),
        icon: Some("share".into()),
        side: SidebarSide::Left,
        component: ComponentRef::Declarative {
            fields: vec![
                SettingsField::Section {
                    label: "Host Session".into(),
                    description: Some(
                        "Create a live session for your current workspace.".into(),
                    ),
                },
                SettingsField::Toggle {
                    key: "share_read_only".into(),
                    label: "Read-only".into(),
                    description: Some("Guests can view but not edit when enabled.".into()),
                },
                SettingsField::Button {
                    label: "Create Session".into(),
                    command: "CreateShareSession".into(),
                    variant: None,
                },
                SettingsField::Button {
                    label: "Apply Access".into(),
                    command: "SetShareReadOnly".into(),
                    variant: Some("outline".into()),
                },
                SettingsField::Button {
                    label: "End Session".into(),
                    command: "EndShareSession".into(),
                    variant: Some("destructive".into()),
                },
                SettingsField::Section {
                    label: "Join Session".into(),
                    description: Some("Paste a join code to enter as a guest.".into()),
                },
                SettingsField::Text {
                    key: "active_join_code".into(),
                    label: "Join Code".into(),
                    description: Some("Use an existing code or copy the active session code.".into()),
                    placeholder: Some("ABCD1234".into()),
                },
                SettingsField::Button {
                    label: "Join Session".into(),
                    command: "JoinShareSession".into(),
                    variant: Some("outline".into()),
                },
            ],
        },
    };

    GuestManifest::new(
        "diaryx.share",
        "Live Share",
        env!("CARGO_PKG_VERSION"),
        "Real-time guest sharing for Diaryx workspaces",
        vec![
            "workspace_events".into(),
            "file_events".into(),
            "sync_transport".into(),
            "custom_commands".into(),
        ],
    )
    .ui(vec![
        serde_json::to_value(&share_settings_tab).unwrap_or_default(),
        serde_json::to_value(&share_tab).unwrap_or_default(),
    ])
    .commands(vec![
        "CreateShareSession".into(),
        "JoinShareSession".into(),
        "EndShareSession".into(),
        "SetShareReadOnly".into(),
        "get_config".into(),
        "set_config".into(),
    ])
    .server_functions(vec![
        ServerFunctionDecl {
            name: "create_session".into(),
            method: "POST".into(),
            path: "/sessions".into(),
            description: "Create a live share session for a namespace".into(),
        },
        ServerFunctionDecl {
            name: "get_session".into(),
            method: "GET".into(),
            path: "/sessions/{code}".into(),
            description: "Look up a session by code (unauthenticated, for guests)".into(),
        },
        ServerFunctionDecl {
            name: "update_session".into(),
            method: "PATCH".into(),
            path: "/sessions/{code}".into(),
            description: "Update session read_only flag (owner only)".into(),
        },
        ServerFunctionDecl {
            name: "delete_session".into(),
            method: "DELETE".into(),
            path: "/sessions/{code}".into(),
            description: "End a session (owner only)".into(),
        },
    ])
    .requested_permissions(GuestRequestedPermissions {
        defaults: serde_json::json!({
            "plugin_storage": { "include": ["all"], "exclude": [] },
            "http_requests": { "include": ["all"], "exclude": [] },
            "read_files": { "include": ["all"], "exclude": [] },
            "edit_files": { "include": ["all"], "exclude": [] },
            "create_files": { "include": ["all"], "exclude": [] },
            "delete_files": { "include": ["all"], "exclude": [] },
            "execute_commands": {
                "include": [
                    "diaryx.sync:PrepareLiveShareRuntime",
                    "diaryx.sync:ConnectLiveShareSession",
                    "diaryx.sync:DisconnectLiveShareSession"
                ],
                "exclude": []
            }
        }),
        reasons: std::collections::HashMap::new(),
    })
}

fn command_response(result: Result<JsonValue, String>) -> CommandResponse {
    match result {
        Ok(data) => CommandResponse::ok(data),
        Err(error) => CommandResponse::err(error),
    }
}

fn execute_command(req: CommandRequest) -> CommandResponse {
    let CommandRequest { command, params } = req;
    let result = match command.as_str() {
        "get_config" => {
            let config = load_extism_config();
            let runtime = load_runtime_context();
            Ok(serde_json::to_value(effective_config(&config, &runtime)).unwrap_or_default())
        }
        "set_config" => {
            let mut config = load_extism_config();
            apply_config_patch(&mut config, &params);
            save_extism_config(&config);
            Ok(JsonValue::Null)
        }
        "CreateShareSession" => handle_create_share_session(&params),
        "JoinShareSession" => handle_join_share_session(&params),
        "FinalizeJoinShareSession" => handle_finalize_join_share_session(&params),
        "EndShareSession" => handle_end_share_session(&params),
        "SetShareReadOnly" => handle_set_share_read_only(&params),
        _ => Err(format!("Unknown command: {command}")),
    };

    command_response(result)
}

#[plugin_fn]
pub fn manifest(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_manifest())?)
}

#[plugin_fn]
pub fn init(input: String) -> FnResult<String> {
    let params: InitParams = serde_json::from_str(&input).unwrap_or_default();
    let mut config = load_extism_config();
    if let Some(server_url) = params.server_url {
        config.server_url = Some(server_url);
    }
    if let Some(auth_token) = params.auth_token {
        config.auth_token = Some(auth_token);
    }
    save_extism_config(&config);

    state::init_state(None).map_err(extism_pdk::Error::msg)?;

    if let Some(root) = params.workspace_root {
        let init_result = state::with_sync_plugin(|sync_plugin| {
            let ctx = diaryx_core::plugin::PluginContext {
                workspace_root: Some(std::path::PathBuf::from(root)),
                link_format: diaryx_core::link_parser::LinkFormat::default(),
            };
            poll_future(diaryx_core::plugin::Plugin::init(sync_plugin, &ctx))
                .map_err(|e| format!("Plugin init failed: {e}"))
        })
        .map_err(extism_pdk::Error::msg)?;
        init_result.map_err(extism_pdk::Error::msg)?;
    }

    Ok(String::new())
}

#[plugin_fn]
pub fn shutdown(_input: String) -> FnResult<String> {
    state::shutdown_state().map_err(extism_pdk::Error::msg)?;
    Ok(String::new())
}

#[plugin_fn]
pub fn handle_command(input: String) -> FnResult<String> {
    let req: CommandRequest = serde_json::from_str(&input)?;
    let response = execute_command(req);
    Ok(serde_json::to_string(&response)?)
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<String> {
    let event: GuestEvent = serde_json::from_str(&input)?;
    let mut session_actions = Vec::new();

    match event.event_type.as_str() {
        "file_saved" | "file_created" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    if let Ok(content) = host::fs::read_file(path) {
                        let body_docs = sync_plugin.body_docs();
                        let doc = body_docs.get_or_create(path);
                        let _ = doc.set_body(frontmatter::extract_body(&content));
                    }
                }) {
                    host::log::log("warn", &format!("[share:on_event:file] {e}"));
                }
            }
        }
        "file_deleted" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    let body_docs = sync_plugin.body_docs();
                    let _ = body_docs.delete(path);
                }) {
                    host::log::log("warn", &format!("[share:on_event:file_deleted] {e}"));
                }
            }
        }
        "file_renamed" | "file_moved" => {
            let old_path = event.payload.get("old_path").and_then(|v| v.as_str());
            let new_path = event.payload.get("new_path").and_then(|v| v.as_str());
            if let (Some(old), Some(new)) = (old_path, new_path) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    let body_docs = sync_plugin.body_docs();
                    let _ = body_docs.rename(old, new);
                }) {
                    host::log::log("warn", &format!("[share:on_event:file_renamed] {e}"));
                }
            }
        }
        "workspace_opened" => {
            if let Some(root) = event.payload.get("workspace_root").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    let event = diaryx_core::plugin::WorkspaceOpenedEvent {
                        workspace_root: std::path::PathBuf::from(root),
                    };
                    poll_future(diaryx_core::plugin::WorkspacePlugin::on_workspace_opened(
                        sync_plugin,
                        &event,
                    ));
                }) {
                    host::log::log(
                        "warn",
                        &format!("[share:on_event:workspace_opened] {e}"),
                    );
                }
            }
        }
        "file_opened" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                session_actions = state::with_session_mut(|session| {
                    poll_future(session.process(IncomingEvent::SyncBodyFiles {
                        file_paths: vec![path.to_string()],
                    }))
                })
                .unwrap_or_else(|e| {
                    host::log::log("warn", &format!("[share:on_event:file_opened] {e}"));
                    None
                })
                .unwrap_or_default();
            }
        }
        _ => {}
    }

    let workspace_root = current_workspace_root();
    execute_session_actions(session_actions, workspace_root.as_deref());
    Ok(String::new())
}

#[plugin_fn]
pub fn get_config(_input: String) -> FnResult<String> {
    let config = load_extism_config();
    let runtime = load_runtime_context();
    Ok(serde_json::to_string(&effective_config(&config, &runtime))?)
}

#[plugin_fn]
pub fn set_config(input: String) -> FnResult<String> {
    let incoming: JsonValue = serde_json::from_str(&input)?;
    let mut config = load_extism_config();
    apply_config_patch(&mut config, &incoming);
    save_extism_config(&config);
    Ok(String::new())
}

#[plugin_fn]
pub fn handle_binary_message(input: Vec<u8>) -> FnResult<Vec<u8>> {
    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::BinaryMessage(input)))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:handle_binary_message] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn handle_text_message(input: String) -> FnResult<Vec<u8>> {
    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::TextMessage(input)))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:handle_text_message] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn on_connected(input: String) -> FnResult<Vec<u8>> {
    if let Ok(params) = serde_json::from_str::<serde_json::Value>(&input)
        && let Some(workspace_id) = params.get("workspace_id").and_then(|v| v.as_str())
    {
        let write_to_disk = params
            .get("write_to_disk")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let _ = state::create_session(workspace_id, write_to_disk);
    }

    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::Connected))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:on_connected] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn on_disconnected(_input: String) -> FnResult<Vec<u8>> {
    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::Disconnected))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:on_disconnected] {e}"));
        None
    })
    .unwrap_or_default();
    let _ = state::persist_state();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn queue_local_update(input: String) -> FnResult<Vec<u8>> {
    let params: JsonValue = serde_json::from_str(&input)?;
    let doc_id = params
        .get("doc_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data_b64 = params.get("data").and_then(|v| v.as_str()).unwrap_or("");

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .unwrap_or_default();

    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::LocalUpdate { doc_id, data }))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:queue_local_update] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn on_snapshot_imported(_input: String) -> FnResult<Vec<u8>> {
    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::SnapshotImported))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:on_snapshot_imported] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

#[plugin_fn]
pub fn sync_body_files(input: String) -> FnResult<Vec<u8>> {
    let params: JsonValue = serde_json::from_str(&input)?;
    let file_paths: Vec<String> = params
        .get("file_paths")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let actions = state::with_session_mut(|session| {
        poll_future(session.process(IncomingEvent::SyncBodyFiles { file_paths }))
    })
    .unwrap_or_else(|e| {
        host::log::log("warn", &format!("[share:sync_body_files] {e}"));
        None
    })
    .unwrap_or_default();
    let workspace_root = current_workspace_root();
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions, workspace_root.as_deref());
    Ok(encoded)
}

fn poll_future<F: std::future::Future>(f: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(f);

    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("Future was not immediately ready in Extism guest"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_exposes_live_share_surface_only() {
        let manifest = build_manifest();
        assert_eq!(manifest.id, "diaryx.share");
        assert!(manifest
            .ui
            .iter()
            .any(|ui| ui.get("slot").and_then(|v| v.as_str()) == Some("SettingsTab")));
        assert!(manifest
            .ui
            .iter()
            .any(|ui| ui.get("id").and_then(|v| v.as_str()) == Some("share")));
        assert!(!manifest
            .commands
            .iter()
            .any(|command| command == "FinalizeJoinShareSession"));
    }

    #[test]
    fn manifest_requests_plugin_bridge_permissions() {
        let manifest = build_manifest();
        let include = manifest
            .requested_permissions
            .as_ref()
            .and_then(|perms| perms.get("defaults"))
            .and_then(|defaults| defaults.get("execute_commands"))
            .and_then(|rule| rule.get("include"))
            .and_then(|include| include.as_array())
            .expect("execute_commands include list should exist");

        assert!(include
            .iter()
            .any(|value| value.as_str() == Some("diaryx.sync:PrepareLiveShareRuntime")));
        assert!(include
            .iter()
            .any(|value| value.as_str() == Some("diaryx.sync:ConnectLiveShareSession")));
        assert!(include
            .iter()
            .any(|value| value.as_str() == Some("diaryx.sync:DisconnectLiveShareSession")));
    }

    #[test]
    fn apply_config_patch_clears_and_sets_values() {
        let mut cfg = ShareExtismConfig {
            server_url: Some("https://old.example".to_string()),
            auth_token: Some("old-token".to_string()),
            active_join_code: Some("oldcode".to_string()),
            share_read_only: Some(true),
            share_role: Some("host".to_string()),
            session_workspace_id: Some("workspace-1".to_string()),
            created_session_workspace: false,
        };

        let patch = serde_json::json!({
            "server_url": null,
            "auth_token": "  ",
            "active_join_code": " newcode ",
            "share_read_only": null,
            "share_role": "guest",
            "session_workspace_id": " workspace-2 "
        });
        apply_config_patch(&mut cfg, &patch);

        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.auth_token, None);
        assert_eq!(cfg.active_join_code.as_deref(), Some("NEWCODE"));
        assert_eq!(cfg.share_read_only, None);
        assert_eq!(cfg.share_role.as_deref(), Some("guest"));
        assert_eq!(cfg.session_workspace_id.as_deref(), Some("workspace-2"));
    }

    #[test]
    fn resolve_auth_token_prefers_runtime_over_stale_config() {
        let config = ShareExtismConfig {
            auth_token: Some("stored-token".to_string()),
            ..ShareExtismConfig::default()
        };
        let runtime = RuntimeContext {
            raw: serde_json::json!({
                "auth_token": "runtime-token"
            }),
            auth_token: Some("runtime-token".to_string()),
            ..RuntimeContext::default()
        };

        assert_eq!(
            resolve_auth_token(&JsonValue::Null, &config, &runtime).as_deref(),
            Some("runtime-token")
        );
    }

    #[test]
    fn resolve_server_url_prefers_runtime_over_stale_config_when_runtime_present() {
        let config = ShareExtismConfig {
            server_url: Some("https://stale.example.com".to_string()),
            ..ShareExtismConfig::default()
        };
        let runtime = RuntimeContext {
            raw: serde_json::json!({
                "server_url": "https://runtime.example.com/sync2"
            }),
            server_url: Some("https://runtime.example.com/sync2".to_string()),
            ..RuntimeContext::default()
        };

        assert_eq!(
            resolve_server_url(&JsonValue::Null, &config, &runtime).as_deref(),
            Ok("https://runtime.example.com")
        );
    }

    #[test]
    fn runtime_sync_workspace_id_reads_provider_links_before_legacy_metadata() {
        let runtime = RuntimeContext {
            current_workspace: Some(RuntimeWorkspace {
                local_id: Some("local-1".to_string()),
                name: Some("Journal".to_string()),
                path: Some("/tmp/journal".to_string()),
                plugin_metadata: Some(std::collections::HashMap::from([(
                    "diaryx.sync".to_string(),
                    serde_json::json!({
                        "serverId": "legacy-workspace"
                    }),
                )])),
                provider_links: vec![RuntimeProviderLink {
                    plugin_id: Some("diaryx.sync".to_string()),
                    remote_workspace_id: Some("provider-linked-workspace".to_string()),
                    sync_enabled: Some(true),
                }],
            }),
            ..RuntimeContext::default()
        };

        assert_eq!(
            runtime_sync_workspace_id(&runtime).as_deref(),
            Some("provider-linked-workspace")
        );
    }

    #[test]
    fn effective_config_uses_runtime_server_without_exposing_auth_token() {
        let config = ShareExtismConfig {
            server_url: Some("https://stale.example.com".to_string()),
            auth_token: Some("stored-token".to_string()),
            active_join_code: Some("ABC123".to_string()),
            share_read_only: Some(true),
            share_role: Some("host".to_string()),
            session_workspace_id: Some("workspace-1".to_string()),
            created_session_workspace: true,
        };
        let runtime = RuntimeContext {
            raw: serde_json::json!({
                "server_url": "https://runtime.example.com/sync"
            }),
            server_url: Some("https://runtime.example.com/sync".to_string()),
            ..RuntimeContext::default()
        };

        assert_eq!(
            effective_config(&config, &runtime),
            ShareExtismConfig {
                server_url: Some("https://runtime.example.com".to_string()),
                auth_token: None,
                active_join_code: Some("ABC123".to_string()),
                share_read_only: Some(true),
                share_role: Some("host".to_string()),
                session_workspace_id: Some("workspace-1".to_string()),
                created_session_workspace: true,
            }
        );
    }
}
