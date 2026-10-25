use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

const API_BASE: &str = "https://api.gofile.io";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const DEFAULT_SALT: &str = "12af056dacea0b";
const SALT_TTL: Duration = Duration::from_secs(3600);

static CACHED_SALT: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
static CACHED_ACCOUNT_TOKEN: LazyLock<tokio::sync::Mutex<Option<String>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(None));

/// Wrapper around the gofile.io API: token creation, content tree discovery and downloads.
pub struct GofileDownloader {
    http: reqwest::Client,
    account_token: String,
    timeout: Duration,
}

impl GofileDownloader {
    pub async fn new(token: Option<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .build()
            .context("failed to build the gofile.io HTTP client")?;

        let account_token = match token {
            Some(token) if !token.trim().is_empty() => token,
            _ => get_or_create_account_token(&http, timeout).await?,
        };

        Ok(Self {
            http,
            account_token,
            timeout,
        })
    }

    async fn content_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue};

        let website_token =
            generate_website_token(&self.http, &self.account_token, self.timeout).await;
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&website_token) {
            headers.insert("X-Website-Token", value);
        }
        headers.insert("X-BL", HeaderValue::from_static("en-US"));
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", self.account_token)) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        headers
    }

    async fn get_content(&self, content_id: &str, password_hash: Option<&str>) -> Result<Value> {
        let mut url = format!(
            "{API_BASE}/contents/{content_id}?page=1&pageSize=100&sortField=name&sortDirection=1"
        );
        if let Some(hash) = password_hash {
            url.push_str("&password=");
            url.push_str(hash);
        }

        const RETRIES: usize = 3;
        for attempt in 0..RETRIES {
            let headers = self.content_headers().await;
            let response = self
                .http
                .get(&url)
                .headers(headers)
                .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
                .timeout(self.timeout)
                .send()
                .await
                .context("gofile.io content request failed")?;

            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt + 1 < RETRIES {
                    tokio::time::sleep(Duration::from_secs(3 * (attempt as u64 + 1))).await;
                    continue;
                }
                bail!("Gofile API rate limit reached (HTTP 429). Please try again shortly.");
            }

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                bail!("gofile.io returned HTTP {status}: {}", truncate(&body, 300));
            }

            let payload: Value = response
                .json()
                .await
                .context("invalid JSON from the gofile.io API")?;
            let data = payload.get("data").cloned().unwrap_or(Value::Null);

            if let Some(password_status) = data.get("passwordStatus").and_then(Value::as_str) {
                if password_status != "passwordOk" {
                    bail!("This gofile.io content is password protected.");
                }
            }

            return Ok(data);
        }

        bail!("Failed to fetch gofile.io content after retries.")
    }

    pub async fn list_files(
        &self,
        content_id: &str,
        password: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        let password_hash = password.map(|password| {
            let digest = Sha256::digest(password.as_bytes());
            hex::encode(digest)
        });

        let mut files = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = vec![content_id.to_string()];

        while let Some(cid) = stack.pop() {
            if !visited.insert(cid.clone()) {
                continue;
            }

            let data = self.get_content(&cid, password_hash.as_deref()).await?;
            if data.is_null() {
                continue;
            }

            let kind = data.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "file" {
                let name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&cid)
                    .to_string();
                let link = data
                    .get("link")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                files.push((name, link));
                continue;
            }

            let Some(children) = data.get("children").and_then(Value::as_object) else {
                continue;
            };
            for child in children.values() {
                if child.get("type").and_then(Value::as_str) == Some("folder") {
                    if let Some(id) = child.get("id").and_then(Value::as_str) {
                        stack.push(id.to_string());
                    }
                } else {
                    files.push((
                        child
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        child
                            .get("link")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ));
                }
            }
        }

        Ok(files)
    }

    pub async fn download_file(&self, link: &str, dest: &Path) -> Result<()> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let response = self
            .http
            .get(link)
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.account_token),
            )
            .send()
            .await
            .context("gofile.io file download failed")?
            .error_for_status()
            .context("gofile.io file download returned an error status")?;

        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("failed to create {}", dest.display()))?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("failed while reading the gofile.io download")?;
            file.write_all(&chunk)
                .await
                .context("failed while writing the downloaded file")?;
        }
        file.flush().await.context("failed to flush the file")?;
        Ok(())
    }
}

async fn get_or_create_account_token(http: &reqwest::Client, timeout: Duration) -> Result<String> {
    {
        let cached = CACHED_ACCOUNT_TOKEN.lock().await;
        if let Some(token) = cached.as_ref() {
            return Ok(token.clone());
        }
    }

    let website_token = generate_website_token(http, "", timeout).await;
    let response = http
        .post(format!("{API_BASE}/accounts"))
        .header("X-Website-Token", website_token)
        .header("X-BL", "en-US")
        .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
        .timeout(timeout)
        .send()
        .await
        .context("gofile.io account creation request failed")?;

    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Gofile account creation rate limited. Please set GF_TOKEN in configuration.");
    }

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "gofile.io account creation returned HTTP {status}: {}",
            truncate(&body, 300)
        );
    }

    let payload: Value = response
        .json()
        .await
        .context("invalid JSON from the gofile.io account API")?;
    if payload.get("status").and_then(Value::as_str) != Some("ok") {
        bail!(
            "Failed to create a gofile.io account token: {}",
            payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
    }

    let token = payload
        .pointer("/data/token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("gofile.io account response did not contain a token"))?
        .to_string();

    *CACHED_ACCOUNT_TOKEN.lock().await = Some(token.clone());
    Ok(token)
}

async fn generate_website_token(
    http: &reqwest::Client,
    account_token: &str,
    timeout: Duration,
) -> String {
    let time_slot = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 14_400;
    let salt = fetch_current_salt(http, timeout).await;
    let raw = format!("{DEFAULT_USER_AGENT}::en-US::{account_token}::{time_slot}::{salt}");
    let digest = Sha256::digest(raw.as_bytes());
    hex::encode(digest)
}

async fn fetch_current_salt(http: &reqwest::Client, timeout: Duration) -> String {
    if let Some((salt, cached_at)) = CACHED_SALT.lock().ok().and_then(|guard| guard.clone()) {
        if cached_at.elapsed() < SALT_TTL {
            return salt;
        }
    }

    if let Ok(salt) = std::env::var("GOFILE_WT_SALT") {
        let salt = salt.trim().to_string();
        if !salt.is_empty() {
            cache_salt(&salt);
            return salt;
        }
    }

    match fetch_dynamic_salt(http, timeout).await {
        Ok(Some(salt)) => {
            cache_salt(&salt);
            salt
        }
        Ok(None) => {
            warn!("Could not locate a salt in gofile's wt.obf.js; using the default");
            cache_salt(DEFAULT_SALT);
            DEFAULT_SALT.to_string()
        }
        Err(err) => {
            warn!("Could not extract dynamic salt from gofile.io: {err:#}");
            cache_salt(DEFAULT_SALT);
            DEFAULT_SALT.to_string()
        }
    }
}

fn cache_salt(salt: &str) {
    if let Ok(mut guard) = CACHED_SALT.lock() {
        *guard = Some((salt.to_string(), Instant::now()));
    }
}

async fn fetch_dynamic_salt(http: &reqwest::Client, timeout: Duration) -> Result<Option<String>> {
    let code = http
        .get("https://gofile.io/js/wt.obf.js")
        .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
        .timeout(timeout)
        .send()
        .await
        .context("failed to fetch wt.obf.js")?
        .error_for_status()
        .context("gofile.io returned an error for wt.obf.js")?
        .text()
        .await
        .context("failed to read wt.obf.js")?;

    const ARRAY_PREFIX: &str = "var _0x145a46=";
    let start = code
        .find("var _0x145a46=[")
        .map(|index| index + ARRAY_PREFIX.len())
        .ok_or_else(|| anyhow!("array marker not found"))?;
    let rest = &code[start..];
    let end = rest
        .find("];a0_0x38b3=")
        .map(|index| index + 1)
        .ok_or_else(|| anyhow!("array terminator not found"))?;

    let mut values = parse_js_string_array(&rest[..end])?;
    if values.is_empty() {
        return Ok(None);
    }

    for _ in 0..values.len() {
        let v1 = decrypt_number(&values, 0x1c6, "a$2m");
        let v2 = decrypt_number(&values, 0x1e3, "1ARU");
        let v3 = decrypt_number(&values, 0x1f6, "FzMC");
        let v4 = decrypt_number(&values, 0x219, "0$(%");
        let v5 = decrypt_number(&values, 0x1d9, "xXED");
        let v6 = decrypt_number(&values, 0x229, "8K]F");
        let v7 = decrypt_number(&values, 0x1f9, "pWr#");
        let v8 = decrypt_number(&values, 0x1f7, "Mioo");
        let v9 = decrypt_number(&values, 0x1ee, "R5mm");

        if let (
            Some(v1),
            Some(v2),
            Some(v3),
            Some(v4),
            Some(v5),
            Some(v6),
            Some(v7),
            Some(v8),
            Some(v9),
        ) = (v1, v2, v3, v4, v5, v6, v7, v8, v9)
        {
            let calc = v1.div_euclid(1) * v2.div_euclid(2) + v3.div_euclid(3) - v4.div_euclid(4)
                + v5.div_euclid(5) * v6.div_euclid(6)
                + v7.div_euclid(7)
                - v8.div_euclid(8)
                - v9.div_euclid(9);
            if calc == 0xed5d1 {
                break;
            }
        }

        values.rotate_left(1);
    }

    let function_start = match code.find("function generateWT") {
        Some(index) => index,
        None => return Ok(None),
    };
    let function_end = code[function_start..]
        .find("window[")
        .map(|offset| function_start + offset)
        .unwrap_or(code.len());
    let generate_slice = &code[function_start..function_end];

    let regex =
        Regex::new(r#"a0_0x1b44\((0x[0-9a-fA-F]+),\s*['"]([^'"]+)['"]\)"#).expect("valid regex");

    for captures in regex.captures_iter(generate_slice) {
        let Some(index) = captures.get(1) else {
            continue;
        };
        let hex_index = index.as_str().trim_start_matches("0x");
        let Ok(index) = usize::from_str_radix(hex_index, 16) else {
            continue;
        };
        let Some(key) = captures.get(2) else {
            continue;
        };
        let key = decode_unicode_escapes(key.as_str());

        if let Some(value) = decrypt_string(&values, index, &key) {
            if value.len() == 14
                && value
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Ok(Some(value));
            }
        }
    }

    Ok(None)
}

fn decrypt_string(values: &[String], index: usize, key: &str) -> Option<String> {
    let value = values.get(index.checked_sub(0x188)?)?;
    let decoded = js_b64(value);
    let decrypted = rc4(&decoded, key.as_bytes());
    Some(String::from_utf8_lossy(&decrypted).into_owned())
}

fn decrypt_number(values: &[String], index: usize, key: &str) -> Option<i64> {
    let value = decrypt_string(values, index, key)?;
    parse_js_int(&value)
}

/// JavaScript's `parseInt` on the leading decimal number of a string.
fn parse_js_int(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let mut chars = value.char_indices();
    let mut end = 0;
    for (index, character) in chars.by_ref() {
        if index == 0 && (character == '+' || character == '-') {
            end = character.len_utf8();
            continue;
        }
        if character.is_ascii_digit() {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    value[..end].parse().ok()
}

/// Obfuscated base64 decoder used by gofile's `wt.obf.js`.
fn js_b64(input: &str) -> Vec<u8> {
    const TABLE: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/=";

    let mut decoded = Vec::with_capacity(input.len());
    let mut position: usize = 0;
    let mut accumulator: u64 = 0;

    for byte in input.bytes() {
        let Some(value) = TABLE.iter().position(|candidate| *candidate == byte) else {
            continue;
        };
        let value = value as u64;

        if !position.is_multiple_of(4) {
            accumulator = accumulator * 64 + value;
        } else {
            accumulator = value;
        }

        let previous = position;
        position += 1;

        if !previous.is_multiple_of(4) {
            let shift = ((-2i64 * position as i64) & 6) as u32;
            decoded.push(((accumulator >> shift) & 0xff) as u8);
        }
    }

    String::from_utf8_lossy(&decoded).into_owned().into_bytes()
}

fn rc4(data: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }

    let mut state = [0u8; 256];
    for (index, value) in state.iter_mut().enumerate() {
        *value = index as u8;
    }

    let mut j = 0usize;
    for i in 0..256 {
        j = (j + state[i] as usize + key[i % key.len()] as usize) % 256;
        state.swap(i, j);
    }

    let mut i = 0usize;
    j = 0;
    let mut output = Vec::with_capacity(data.len());
    for &byte in data {
        i = (i + 1) % 256;
        j = (j + state[i] as usize) % 256;
        state.swap(i, j);
        let key_byte = state[(state[i] as usize + state[j] as usize) % 256];
        output.push(byte ^ key_byte);
    }
    output
}

fn parse_js_string_array(input: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0usize;

    skip_whitespace(&chars, &mut index);
    if chars.get(index) != Some(&'[') {
        bail!("expected '[' at the start of the embedded array");
    }
    index += 1;

    let mut values = Vec::new();
    loop {
        skip_whitespace(&chars, &mut index);
        match chars.get(index) {
            Some(']') => {
                break;
            }
            Some(',') => {
                index += 1;
            }
            Some(&quote) if quote == '\'' || quote == '"' => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(&character) = chars.get(index) else {
                        bail!("unterminated string in the embedded array");
                    };
                    index += 1;

                    if character == quote {
                        break;
                    }
                    if character != '\\' {
                        value.push(character);
                        continue;
                    }

                    let Some(&escaped) = chars.get(index) else {
                        bail!("unterminated escape in the embedded array");
                    };
                    index += 1;
                    match escaped {
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        'b' => value.push('\u{0008}'),
                        'f' => value.push('\u{000c}'),
                        'v' => value.push('\u{000b}'),
                        '0' => value.push('\0'),
                        '\\' => value.push('\\'),
                        '\'' => value.push('\''),
                        '"' => value.push('"'),
                        '\n' => {}
                        'x' => {
                            let code = take_hex(&chars, &mut index, 2)?;
                            value.push(
                                char::from_u32(code)
                                    .ok_or_else(|| anyhow!("invalid \\x escape"))?,
                            );
                        }
                        'u' => {
                            let code = take_hex(&chars, &mut index, 4)?;
                            value.push(
                                char::from_u32(code)
                                    .ok_or_else(|| anyhow!("invalid \\u escape"))?,
                            );
                        }
                        other => value.push(other),
                    }
                }
                values.push(value);
            }
            Some(other) => bail!("unexpected character {other:?} in the embedded array"),
            None => bail!("unterminated embedded array"),
        }
    }

    Ok(values)
}

fn skip_whitespace(chars: &[char], index: &mut usize) {
    while chars.get(*index).is_some_and(|c| c.is_whitespace()) {
        *index += 1;
    }
}

fn take_hex(chars: &[char], index: &mut usize, count: usize) -> Result<u32> {
    let mut value = 0u32;
    for _ in 0..count {
        let Some(&character) = chars.get(*index) else {
            bail!("truncated hex escape");
        };
        let Some(digit) = character.to_digit(16) else {
            bail!("invalid hex escape");
        };
        value = value * 16 + digit;
        *index += 1;
    }
    Ok(value)
}

fn decode_unicode_escapes(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }

    let chars: Vec<char> = value.chars().collect();
    let mut index = 0usize;
    let mut output = String::new();
    while let Some(&character) = chars.get(index) {
        index += 1;
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(&escaped) = chars.get(index) else {
            break;
        };
        index += 1;
        match escaped {
            'x' => {
                if let Ok(code) = take_hex(&chars, &mut index, 2) {
                    if let Some(character) = char::from_u32(code) {
                        output.push(character);
                    }
                }
            }
            'u' => {
                if let Ok(code) = take_hex(&chars, &mut index, 4) {
                    if let Some(character) = char::from_u32(code) {
                        output.push(character);
                    }
                }
            }
            other => output.push(other),
        }
    }
    output
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

pub fn extract_content_id(url: &str) -> Result<&str> {
    let trimmed = url.trim_end_matches('/');
    let mut parts = trimmed.rsplit('/');
    let content_id = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("Not a valid gofile.io content url: {url}"))?;
    if parts.next() != Some("d") {
        bail!("Not a valid gofile.io content url: {url}");
    }
    Ok(content_id)
}

pub fn pick_boot_img(files: &[(String, String)]) -> Option<(&str, &str)> {
    files
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("boot.img"))
        .or_else(|| {
            files
                .iter()
                .find(|(name, _)| name.to_ascii_lowercase().ends_with(".img"))
        })
        .map(|(name, link)| (name.as_str(), link.as_str()))
}

/// Resolves a gofile.io link, finds a `boot.img` (or the first `.img`) and downloads it.
pub async fn download_bootimg(
    url: &str,
    dest: &Path,
    password: Option<&str>,
    token: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let content_id = extract_content_id(url)?;
    let downloader = GofileDownloader::new(token.map(str::to_string), timeout).await?;
    let files = downloader.list_files(content_id, password).await?;
    if files.is_empty() {
        bail!("No files found in the gofile.io content.");
    }

    let (name, link) = pick_boot_img(&files)
        .ok_or_else(|| anyhow!("No boot.img file found in the gofile.io content."))?;
    info!("Downloading {name} from gofile.io");
    downloader.download_file(link, dest).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_matches_the_reference_wikipedia_vector() {
        assert_eq!(hex::encode(rc4(b"Plaintext", b"Key")), "bbf316e8d940af0ad3");
    }

    #[test]
    fn parses_single_and_double_quoted_arrays() {
        let values = parse_js_string_array(r#"['abc', "de'f", 'a\x41']"#).unwrap();
        assert_eq!(values, vec!["abc", "de'f", "aA"]);
    }

    #[test]
    fn extracts_content_ids() {
        assert_eq!(
            extract_content_id("https://gofile.io/d/abc123/").unwrap(),
            "abc123"
        );
        assert!(extract_content_id("https://gofile.io/abc123").is_err());
    }

    #[test]
    fn prefers_boot_img_then_any_img() {
        let files = vec![
            ("other.img".to_string(), "1".to_string()),
            ("BOOT.IMG".to_string(), "2".to_string()),
        ];
        assert_eq!(pick_boot_img(&files), Some(("BOOT.IMG", "2")));

        let files = vec![
            ("readme.txt".to_string(), "0".to_string()),
            ("other.img".to_string(), "1".to_string()),
        ];
        assert_eq!(pick_boot_img(&files), Some(("other.img", "1")));
    }
}
