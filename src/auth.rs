use axum::{
    extract::{Query, Request},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::LazyLock;

use crate::config;

const SESSION_COOKIE: &str = "monitor_session";
const SESSION_MAX_AGE: u64 = 60 * 60 * 24; // 24 hours

// In-memory session store: session_id -> email
static SESSIONS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn generate_session_id() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    hex::encode(bytes)
}

fn generate_state() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::rng().random();
    hex::encode(bytes)
}

// PKCE S256 code verifier + challenge
fn generate_pkce() -> (String, String) {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    let verifier = hex::encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64_url_encode(&hash);
    (verifier, challenge)
}

fn base64_url_encode(data: &[u8]) -> String {
    use base64_encode::encode;
    encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

// Simple base64 without pulling in the base64 crate
mod base64_encode {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut result = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let triple = (b0 << 16) | (b1 << 8) | b2;
            result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
            result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
            if chunk.len() > 2 {
                result.push(CHARS[(triple & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
        }
        result
    }
}

// Pending OIDC flows: state -> (code_verifier)
static PENDING_FLOWS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn get_session_email(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let session_id = cookie_header
        .split(';')
        .find_map(|pair| {
            let pair = pair.trim();
            let (k, v) = pair.split_once('=')?;
            if k.trim() == SESSION_COOKIE {
                Some(v.trim().to_string())
            } else {
                None
            }
        })?;

    let sessions = SESSIONS.lock().unwrap();
    sessions.get(&session_id).cloned()
}

pub async fn auth_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();

    // Allow health endpoint and auth routes without auth
    if path == "/api/health" || path.starts_with("/auth/") {
        return next.run(request).await;
    }

    if get_session_email(request.headers()).is_some() {
        return next.run(request).await;
    }

    // Not authenticated — redirect to login
    let cfg = config::get();
    let state = generate_state();
    let (verifier, challenge) = generate_pkce();

    PENDING_FLOWS.lock().unwrap().insert(state.clone(), verifier);

    let authorize_url = format!(
        "{}/authorize?client_id={}&redirect_uri={}&response_type=code&scope=openid+email&state={}&code_challenge={}&code_challenge_method=S256",
        cfg.oidc_issuer,
        cfg.oidc_client_id,
        urlenc(&cfg.oidc_redirect_uri),
        state,
        challenge,
    );

    Redirect::temporary(&authorize_url).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

pub async fn auth_callback(Query(q): Query<CallbackQuery>) -> impl IntoResponse {
    let cfg = config::get();

    // Retrieve and remove the pending flow
    let verifier = {
        let mut flows = PENDING_FLOWS.lock().unwrap();
        flows.remove(&q.state)
    };

    let Some(verifier) = verifier else {
        return (StatusCode::BAD_REQUEST, "Invalid state parameter").into_response();
    };

    // Exchange authorization code for tokens
    let client = reqwest::Client::new();
    let token_url = format!("{}/token", cfg.oidc_issuer);

    let res = client
        .post(&token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &q.code),
            ("redirect_uri", &cfg.oidc_redirect_uri),
            ("client_id", &cfg.oidc_client_id),
            ("client_secret", &cfg.oidc_client_secret),
            ("code_verifier", &verifier),
        ])
        .send()
        .await;

    let Ok(res) = res else {
        return (StatusCode::BAD_GATEWAY, "Failed to contact IDP").into_response();
    };

    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        tracing::error!(body, "token exchange failed");
        return (StatusCode::UNAUTHORIZED, "Token exchange failed").into_response();
    }

    let Ok(token_response) = res.json::<serde_json::Value>().await else {
        return (StatusCode::BAD_GATEWAY, "Invalid token response").into_response();
    };

    // Extract email from ID token claims (we trust our own IDP)
    let email = extract_email_from_id_token(&token_response).unwrap_or_else(|| "unknown".to_string());

    // Create session
    let session_id = generate_session_id();
    SESSIONS.lock().unwrap().insert(session_id.clone(), email.clone());

    tracing::info!(email, "user authenticated via OIDC");

    let cookie = format!(
        "{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_MAX_AGE}"
    );

    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/".to_string()),
        ],
    )
        .into_response()
}

pub async fn auth_logout() -> impl IntoResponse {
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/".to_string()),
        ],
    )
}

fn extract_email_from_id_token(token_response: &serde_json::Value) -> Option<String> {
    let id_token = token_response.get("id_token")?.as_str()?;
    // Decode JWT payload (second segment) — we trust our own IDP, no signature verification needed
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64_url_decode(parts[1])?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut s = input.replace('-', "+").replace('_', "/");
    while s.len() % 4 != 0 {
        s.push('=');
    }
    base64_decode::decode(&s)
}

mod base64_decode {
    pub fn decode(input: &str) -> Option<Vec<u8>> {
        let table: [u8; 128] = {
            let mut t = [255u8; 128];
            for (i, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
                t[c as usize] = i as u8;
            }
            t[b'=' as usize] = 0;
            t
        };

        let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();
        let mut result = Vec::with_capacity(bytes.len() * 3 / 4);

        for chunk in bytes.chunks(4) {
            if chunk.len() != 4 {
                return None;
            }
            let vals: Vec<u8> = chunk
                .iter()
                .map(|&b| if b < 128 { table[b as usize] } else { 255 })
                .collect();
            if vals.iter().any(|&v| v == 255) {
                return None;
            }
            let triple = ((vals[0] as u32) << 18) | ((vals[1] as u32) << 12) | ((vals[2] as u32) << 6) | (vals[3] as u32);
            result.push((triple >> 16) as u8);
            if chunk[2] != b'=' {
                result.push((triple >> 8) as u8);
            }
            if chunk[3] != b'=' {
                result.push(triple as u8);
            }
        }

        Some(result)
    }
}

fn urlenc(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}
