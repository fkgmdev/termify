use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, Utc};
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::{AuthCodePkceSpotify, Credentials, OAuth, Token};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};
use url::Url;

use crate::config::{Config, Paths};

/// Spotify's desktop client ID. Third-party client IDs can use the Web API but
/// cannot obtain the private credentials librespot needs for audio keys, so
/// local playback uses this ID with the device-pairing flow.
const DESKTOP_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const DEVICE_AUTH_URL: &str = "https://accounts.spotify.com/oauth2/device/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

const WEB_SCOPES: &[&str] = &[
    "user-read-private",
    "user-read-email",
    "user-library-read",
    "playlist-read-private",
    "playlist-read-collaborative",
    // Required by the playlist editing commands (create/rename/add/remove/reorder).
    "playlist-modify-private",
    "playlist-modify-public",
];
const PLAYBACK_SCOPES: &str = "streaming";

/// Persist a token with owner-only permissions. The whole token is rewritten
/// after every refresh, preserving the previous refresh token when Spotify does
/// not issue a new one.
pub fn save_token(path: &std::path::Path, token: &Token) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options
        .open(path)
        .with_context(|| format!("opening token cache {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, token)?;
    file.flush()?;
    Ok(())
}

fn scope_set(scopes_list: &[&str]) -> HashSet<String> {
    scopes_list.iter().map(|s| (*s).to_string()).collect()
}

async fn client_from_cache(
    path: &std::path::Path,
    credentials: Credentials,
    oauth: OAuth,
    required_scopes: &HashSet<String>,
) -> Result<Option<AuthCodePkceSpotify>> {
    let token = match Token::from_cache(path) {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!("Ignoring unreadable token cache ({}): {err}", path.display());
            return Ok(None);
        }
    };
    // Refresh tokens cannot gain new scopes. If the cached token predates the
    // playlist write scopes, fall through to a fresh interactive login.
    if !required_scopes.is_subset(&token.scopes) {
        tracing::info!("Cached token is missing required scopes; re-authorizing");
        return Ok(None);
    }
    let client = AuthCodePkceSpotify::from_token_with_config(
        token,
        credentials,
        oauth,
        rspotify::Config::default(),
    );
    let (expired, previous_refresh_token) = {
        let guard = client
            .token
            .lock()
            .await
            .map_err(|_| anyhow::anyhow!("token mutex lock failed"))?;
        let Some(existing) = guard.as_ref() else {
            return Ok(None);
        };
        (existing.is_expired(), existing.refresh_token.clone())
    };
    if expired {
        tracing::info!("Access token expired; refreshing {}", path.display());
        if let Err(err) = client.refresh_token().await {
            // Invalid/stale refresh tokens (including the 400 Spotify returns
            // when the cached token belongs to a different app or redirect URI)
            // must not abort startup. Fall through to a fresh login instead.
            tracing::warn!(
                "Could not refresh cached token ({}): {err}; starting a new login",
                path.display()
            );
            return Ok(None);
        }
        let mut refreshed = client
            .token
            .lock()
            .await
            .map_err(|_| anyhow::anyhow!("token mutex lock failed"))?
            .clone()
            .context("no token after refresh")?;
        // Spotify may omit `refresh_token` from a refresh response. rspotify
        // replaces the whole token, so restore the old refresh token instead of
        // losing the ability to renew on the next launch.
        if refreshed.refresh_token.is_none() {
            if previous_refresh_token.is_some() {
                tracing::info!("Refresh response omitted refresh token; reusing cached one");
            }
            refreshed.refresh_token = previous_refresh_token;
        }
        {
            let mut guard = client
                .token
                .lock()
                .await
                .map_err(|_| anyhow::anyhow!("token mutex lock failed"))?;
            *guard = Some(refreshed.clone());
        }
        save_token(path, &refreshed)?;
    }
    Ok(Some(client))
}

async fn access_token(client: &AuthCodePkceSpotify) -> Result<String> {
    Ok(client
        .get_token()
        .lock()
        .await
        .map_err(|_| anyhow::anyhow!("token mutex lock failed"))?
        .as_ref()
        .context("missing access token")?
        .access_token
        .clone())
}

// ---------------------------------------------------------------------------
// Web API client: Authorization Code + PKCE with the user's own app/client ID.
// ---------------------------------------------------------------------------

fn web_credentials(config: &Config) -> Credentials {
    Credentials::new_pkce(&config.client_id)
}

fn web_oauth(config: &Config) -> OAuth {
    OAuth {
        redirect_uri: config.redirect_uri.clone(),
        scopes: scope_set(WEB_SCOPES),
        ..Default::default()
    }
}

/// Return an authenticated Web API client, using the cached PKCE token when
/// possible and running the interactive loopback flow otherwise.
pub async fn authenticated(
    config: &Config,
    paths: &Paths,
    force_login: bool,
) -> Result<AuthCodePkceSpotify> {
    if !force_login && paths.token_file.exists() {
        if let Some(client) = client_from_cache(
            &paths.token_file,
            web_credentials(config),
            web_oauth(config),
            &scope_set(WEB_SCOPES),
        )
        .await?
        {
            return Ok(client);
        }
        println!("Re-authorizing with Spotify...");
    }
    web_login(config, paths).await
}

async fn web_login(config: &Config, paths: &Paths) -> Result<AuthCodePkceSpotify> {
    let mut client = AuthCodePkceSpotify::new(web_credentials(config), web_oauth(config));
    let authorize_url = client.get_authorize_url(None)?;

    let redirect = Url::parse(&config.redirect_uri)
        .with_context(|| format!("invalid redirect URI: {}", config.redirect_uri))?;
    let host = redirect.host_str().context("redirect URI has no host")?;
    let port = redirect
        .port_or_known_default()
        .context("redirect URI has no port")?;
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        bail!("redirect URI host must be loopback, got {host}");
    }
    let listen_addr = format!("{host}:{port}");

    // Bind before opening the browser so the callback cannot be missed.
    let listener = TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("binding {listen_addr} for the OAuth callback"))?;

    let callback = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 8192];
        let len = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..len]).into_owned();
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .context("malformed OAuth callback request")?
            .to_string();

        let page = "<!doctype html><meta charset=utf-8><title>termify</title>\
                    <style>body{font:16px system-ui;background:#121212;color:#fff;\
                    display:grid;place-items:center;height:100vh;margin:0}</style>\
                    <p>Authenticated. You can close this window and return to termify.</p>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            page.len(),
            page
        );
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        Ok::<_, anyhow::Error>(format!("http://{listen_addr}{target}"))
    });

    println!("Opening Spotify authorization for Web API access in your browser...");
    println!("If it does not open automatically, visit:\n{authorize_url}");
    if let Err(err) = webbrowser::open(&authorize_url) {
        tracing::warn!("Could not open browser automatically: {err}");
    }

    let callback_url = timeout(Duration::from_secs(300), callback)
        .await
        .context("timed out waiting for Spotify authorization")?
        .context("OAuth callback task failed")??;

    let parsed = Url::parse(&callback_url)?;
    let received_state = parsed
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned());
    if received_state.as_deref() != Some(client.oauth.state.as_str()) {
        bail!("OAuth state mismatch; refusing callback (possible CSRF)");
    }

    let code = client
        .parse_response_code(&callback_url)
        .context("authorization callback did not contain a code")?;
    client
        .request_token(&code)
        .await
        .context("exchanging the authorization code (check Client ID and redirect URI)")?;

    let token = client
        .token
        .lock()
        .await
        .map_err(|_| anyhow::anyhow!("token mutex lock failed"))?
        .clone()
        .context("no token after authorization")?;
    save_token(&paths.token_file, &token)?;
    Ok(client)
}

// ---------------------------------------------------------------------------
// Playback token: device pairing with Spotify's desktop client ID.
// ---------------------------------------------------------------------------

fn playback_credentials() -> Credentials {
    Credentials::new_pkce(DESKTOP_CLIENT_ID)
}

fn playback_oauth() -> OAuth {
    OAuth {
        scopes: scope_set(&[PLAYBACK_SCOPES]),
        ..Default::default()
    }
}

/// Return an access token that librespot can use for local streaming.
pub async fn playback_access_token(paths: &Paths, force_login: bool) -> Result<String> {
    if !force_login && paths.playback_token_file.exists() {
        if let Some(client) = client_from_cache(
            &paths.playback_token_file,
            playback_credentials(),
            playback_oauth(),
            &scope_set(&[PLAYBACK_SCOPES]),
        )
        .await?
        {
            return access_token(&client).await;
        }
    }
    access_token(&device_login(paths).await?).await
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval: Option<u64>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceError {
    error: String,
    error_description: Option<String>,
}

async fn device_login(paths: &Paths) -> Result<AuthCodePkceSpotify> {
    let http = reqwest::Client::new();
    let response = http
        .post(DEVICE_AUTH_URL)
        .form(&[
            ("client_id", DESKTOP_CLIENT_ID),
            ("scope", PLAYBACK_SCOPES),
        ])
        .send()
        .await
        .context("requesting a Spotify device code")?
        .error_for_status()
        .context("Spotify rejected the device authorization request")?;
    let device: DeviceCodeResponse = response
        .json()
        .await
        .context("parsing the Spotify device code response")?;

    let url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    println!("Open this page in your browser to authorize local playback:\n{url}");
    println!("If prompted, enter this code: {}", device.user_code);
    if let Err(err) = webbrowser::open(&url) {
        tracing::warn!("Could not open browser automatically: {err}");
    }

    let mut interval = device.interval.unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    loop {
        if Instant::now() >= deadline {
            bail!("Spotify device authorization expired");
        }
        sleep(Duration::from_secs(interval)).await;

        let response = http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", DESKTOP_CLIENT_ID),
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", device.device_code.as_str()),
            ])
            .send()
            .await
            .context("polling Spotify for a device token")?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("parsing the Spotify device token response")?;

        if status.is_success() && body.get("access_token").is_some() {
            let token = token_from_device_response(&body)?;
            save_token(&paths.playback_token_file, &token)?;
            return Ok(AuthCodePkceSpotify::from_token_with_config(
                token,
                playback_credentials(),
                playback_oauth(),
                rspotify::Config::default(),
            ));
        }

        let error: DeviceError = match serde_json::from_value(body) {
            Ok(error) => error,
            Err(_) => bail!("Spotify device token request failed with HTTP {status}"),
        };
        match error.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            "access_denied" => bail!("Spotify authorization was denied"),
            "expired_token" => bail!("Spotify device authorization expired"),
            other => bail!(
                "Spotify device token exchange failed with HTTP {status}: {other}{}",
                error
                    .error_description
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ),
        }
    }
}

fn token_from_device_response(body: &serde_json::Value) -> Result<Token> {
    let access_token = body["access_token"]
        .as_str()
        .context("device token response has no access_token")?
        .to_string();
    // Spotify does not promise a refresh token on every grant. Keep the access
    // token usable and fall back to a fresh device login when it expires.
    let refresh_token = body["refresh_token"].as_str().map(str::to_string);
    let scope = body["scope"]
        .as_str()
        .context("device token response has no scope")?;
    let scopes: HashSet<String> = scope.split_whitespace().map(str::to_string).collect();
    // Without `streaming` the session may connect but all audio/CDN/key calls
    // fail later, which looks like a download bug instead of an auth problem.
    if !scopes.contains(PLAYBACK_SCOPES) {
        bail!("Spotify device token is missing the '{PLAYBACK_SCOPES}' scope");
    }
    let expires_in = body["expires_in"]
        .as_u64()
        .context("device token response has no valid expires_in")? as i64;

    Ok(Token {
        access_token,
        expires_in: ChronoDuration::seconds(expires_in),
        expires_at: Some(Utc::now() + ChronoDuration::seconds(expires_in)),
        refresh_token,
        scopes,
    })
}
