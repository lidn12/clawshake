//! Authentication middleware for the broker HTTP server.
//!
//! When `auth_token` is configured, every request must present either:
//!   - `Authorization: Bearer <token>` header, or
//!   - `__clawshake_session=<token>` cookie
//!
//! Requests to `GET /ui/login` and `POST /ui/login` are exempt so users can
//! authenticate via browser.  When no `auth_token` is configured, all requests
//! pass through (local-only mode).

use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};

const SESSION_COOKIE: &str = "__clawshake_session";

/// Axum middleware that checks auth when `auth_token` is configured.
pub async fn auth_middleware(
    State(auth_token): State<Option<String>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(ref expected) = auth_token else {
        // No auth configured — pass through.
        return next.run(req).await;
    };

    // Allow login page without auth.
    let path = req.uri().path();
    if path == "/ui/login" {
        return next.run(req).await;
    }

    // Check Authorization header.
    if let Some(header) = req.headers().get("authorization") {
        if let Ok(value) = header.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                if token == expected {
                    return next.run(req).await;
                }
            }
        }
    }

    // Check session cookie.
    if let Some(cookie_header) = req.headers().get("cookie") {
        if let Ok(cookies) = cookie_header.to_str() {
            for part in cookies.split(';') {
                let part = part.trim();
                if let Some(value) = part.strip_prefix(SESSION_COOKIE) {
                    let value = value.strip_prefix('=').unwrap_or(value);
                    if value == expected {
                        return next.run(req).await;
                    }
                }
            }
        }
    }

    // Unauthenticated — redirect browsers to login, return 401 for API calls.
    let accept = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if accept.contains("text/html") {
        (StatusCode::TEMPORARY_REDIRECT, [("location", "/ui/login")], "").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

/// Serve the login page or handle form submission.
pub async fn login_page(req: Request<axum::body::Body>) -> Response {
    if req.method() == axum::http::Method::POST {
        return handle_login_post(req).await;
    }
    Html(LOGIN_HTML).into_response()
}

async fn handle_login_post(req: Request<axum::body::Body>) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };

    // Parse form: token=<value> (simple URL-encoded form)
    let body_str = String::from_utf8_lossy(&body);
    let mut token_value = None;
    for pair in body_str.split('&') {
        if let Some(val) = pair.strip_prefix("token=") {
            // URL-decode the value (tokens are typically alphanumeric, but
            // handle + and %XX just in case).
            let decoded = val
                .replace('+', " ")
                .replace("%26", "&")
                .replace("%3D", "=");
            token_value = Some(decoded);
        }
    }

    let Some(token) = token_value else {
        return (StatusCode::BAD_REQUEST, "missing token field").into_response();
    };

    // Set session cookie and redirect to /ui.
    // The auth middleware will validate the cookie value on subsequent requests.
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"
    );
    (
        StatusCode::SEE_OTHER,
        [("set-cookie", cookie.as_str()), ("location", "/ui")],
        "",
    )
        .into_response()
}

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>clawshake — login</title>
  <style>
    body { font-family: system-ui, sans-serif; background: #1a1a1a; color: #e0e0e0;
           display: flex; align-items: center; justify-content: center;
           min-height: 100vh; margin: 0; }
    form { background: #222; padding: 32px; border-radius: 8px;
           display: flex; flex-direction: column; gap: 16px; min-width: 300px; }
    input { padding: 10px; border: 1px solid #444; border-radius: 4px;
            background: #1a1a1a; color: #e0e0e0; font-size: 16px; }
    button { padding: 10px; border: none; border-radius: 4px;
             background: #4a9eff; color: #fff; font-size: 16px; cursor: pointer; }
    button:hover { background: #3a8eef; }
    h2 { margin: 0; font-size: 18px; }
  </style>
</head>
<body>
  <form method="POST" action="/ui/login">
    <h2>clawshake</h2>
    <input type="password" name="token" placeholder="Access token" required autofocus>
    <button type="submit">Sign in</button>
  </form>
</body>
</html>
"#;
