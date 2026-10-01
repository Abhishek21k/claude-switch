//! Gateway input and saved, non-credential settings.

use crate::atomic;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayInput {
    Keep,
    Remove,
    Url(String),
    Json {
        url: String,
        settings: BTreeMap<String, String>,
        dropped: Vec<String>,
    },
}

pub fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn credential_name(name: &str) -> bool {
    name.split('_').any(|part| {
        matches!(
            part.to_ascii_uppercase().as_str(),
            "TOKEN" | "KEY" | "SECRET" | "PASSWORD" | "CREDENTIAL" | "CREDENTIALS" | "HEADERS"
        )
    })
}

pub fn normalize_base_url(input: &str) -> Result<String> {
    if input.is_empty()
        || input.len() > 2048
        || !input.bytes().all(|b| (0x21..=0x7e).contains(&b))
        || input.contains(['?', '#'])
    {
        bail!("Invalid gateway base URL.");
    }
    let (scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("Invalid gateway base URL."))?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        bail!("Invalid gateway base URL.");
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() || authority.contains('@') {
        bail!("Invalid gateway base URL.");
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("Invalid gateway base URL."))?;
        let host = &authority[..=end];
        let suffix = &authority[end + 1..];
        if host.len() < 4
            || !host[1..host.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':')
            || (!suffix.is_empty() && !suffix.starts_with(':'))
        {
            bail!("Invalid gateway base URL.");
        }
        (host, suffix)
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, ""), |(h, _)| (h, &authority[h.len()..]));
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            bail!("Invalid gateway base URL.");
        }
        (host, port)
    };
    if !port.is_empty() && (port.len() == 1 || !port[1..].bytes().all(|b| b.is_ascii_digit())) {
        bail!("Invalid gateway base URL.");
    }
    let host = host.to_ascii_lowercase();
    if scheme == "http" && !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        bail!("Invalid gateway base URL.");
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        Ok(format!("{scheme}://{host}{port}"))
    } else {
        Ok(format!("{scheme}://{host}{port}/{path}"))
    }
}

pub fn host(url: &str) -> &str {
    let authority = url
        .split_once("://")
        .map_or("", |(_, rest)| rest.split('/').next().unwrap_or(""));
    if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .map_or(authority, |h| &authority[..h.len() + 1])
    } else {
        authority.split(':').next().unwrap_or("")
    }
}

pub fn parse_gateway_input(input: &str) -> Result<GatewayInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(GatewayInput::Keep);
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Ok(GatewayInput::Remove);
    }
    if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|_| anyhow::anyhow!("That isn't a base URL or a settings JSON object."))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("That isn't a base URL or a settings JSON object."))?;
        let settings = match object.get("env") {
            Some(env) => env
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Gateway JSON env must be an object."))?,
            None => object,
        };
        let mut kept = BTreeMap::new();
        let mut dropped = Vec::new();
        for (name, value) in settings {
            if !valid_env_name(name) {
                bail!("Invalid gateway setting name.");
            }
            let Some(value) = value.as_str() else {
                bail!("Gateway setting {name} must be a string.");
            };
            if credential_name(name) {
                dropped.push(name.clone());
            } else {
                kept.insert(name.clone(), value.to_string());
            }
        }
        let Some(url) = kept.get("ANTHROPIC_BASE_URL") else {
            bail!("Gateway JSON needs ANTHROPIC_BASE_URL.");
        };
        let normalized = normalize_base_url(url)?;
        kept.insert("ANTHROPIC_BASE_URL".into(), normalized.clone());
        Ok(GatewayInput::Json {
            url: normalized,
            settings: kept,
            dropped,
        })
    } else {
        normalize_base_url(input)
            .map(GatewayInput::Url)
            .map_err(|_| anyhow::anyhow!("That isn't a base URL or a settings JSON object."))
    }
}

pub type Defaults = BTreeMap<String, BTreeMap<String, String>>;
const STORE_ERROR: &str = "gateways.json is unsafe or invalid; no defaults were changed.";

pub fn read_defaults(base_dir: &Path) -> Result<Defaults> {
    let path = base_dir.join("gateways.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Defaults::new()),
        Err(_) => bail!("{STORE_ERROR}"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{STORE_ERROR}");
    }
    let bytes = fs::read(path).map_err(|_| anyhow::anyhow!("{STORE_ERROR}"))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("{STORE_ERROR}"))?;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        bail!("{STORE_ERROR}");
    }
    let gateways = value
        .get("gateways")
        .ok_or_else(|| anyhow::anyhow!("{STORE_ERROR}"))?;
    let defaults: Defaults =
        serde_json::from_value(gateways.clone()).map_err(|_| anyhow::anyhow!("{STORE_ERROR}"))?;
    for (url, settings) in &defaults {
        if normalize_base_url(url).ok().as_deref() != Some(url)
            || settings.get("ANTHROPIC_BASE_URL") != Some(url)
            || settings
                .keys()
                .any(|name| !valid_env_name(name) || credential_name(name))
        {
            bail!("{STORE_ERROR}");
        }
    }
    Ok(defaults)
}

pub fn write_defaults(base_dir: &Path, defaults: &Defaults) -> Result<()> {
    // Recheck immediately before replacement; never overwrite an unreadable store.
    let _ = read_defaults(base_dir)?;
    atomic::write_private(
        &base_dir.join("gateways.json"),
        &serde_json::to_vec_pretty(&json!({"version": 1, "gateways": defaults}))?,
    )
}

pub fn forget(base_dir: &Path, input: &str) -> Result<String> {
    let url = normalize_base_url(input)?;
    let mut defaults = read_defaults(base_dir)?;
    if defaults.remove(&url).is_none() {
        bail!("No saved defaults for {url}.");
    }
    write_defaults(base_dir, &defaults)?;
    Ok(url)
}

pub fn list(base_dir: &Path) -> Result<String> {
    let defaults = read_defaults(base_dir)?;
    if defaults.is_empty() {
        return Ok("No saved gateway defaults.\n".into());
    }
    let mut output = String::new();
    for (url, settings) in defaults {
        push_wrapped(&mut output, &format!("{url} ({} settings)", settings.len()));
        push_wrapped(
            &mut output,
            &format!(
                "  {}",
                settings.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    Ok(output)
}

fn push_wrapped(output: &mut String, line: &str) {
    let mut remaining = line;
    while remaining.len() > 120 {
        let (head, tail) = remaining.split_at(120);
        output.push_str(head);
        output.push('\n');
        remaining = tail;
    }
    output.push_str(remaining);
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn g1_parses_keep_remove_url_and_both_json_shapes() {
        // Known-bad: requiring env or treating a JSON object as a URL.
        assert_eq!(parse_gateway_input(" ").unwrap(), GatewayInput::Keep);
        assert_eq!(parse_gateway_input("none").unwrap(), GatewayInput::Remove);
        assert_eq!(parse_gateway_input("NONE").unwrap(), GatewayInput::Remove);
        assert_eq!(
            parse_gateway_input("HTTPS://Gateway.Example.Com/Anthropic/").unwrap(),
            GatewayInput::Url("https://gateway.example.com/Anthropic".into())
        );
        for input in [
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com/a/","ANTHROPIC_MODEL":"vendor/claude-model[1m]"}}"#,
            r#"{"ANTHROPIC_BASE_URL":"https://gateway.example.com/a/","ANTHROPIC_MODEL":"vendor/claude-model[1m]"}"#,
        ] {
            let GatewayInput::Json {
                url,
                settings,
                dropped,
            } = parse_gateway_input(input).unwrap()
            else {
                panic!("expected JSON");
            };
            assert_eq!(url, "https://gateway.example.com/a");
            assert_eq!(settings["ANTHROPIC_BASE_URL"], url);
            assert_eq!(settings["ANTHROPIC_MODEL"], "vendor/claude-model[1m]");
            assert!(dropped.is_empty());
        }
        assert!(parse_gateway_input("{}").is_err());
        assert!(parse_gateway_input(r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","API_TIMEOUT_MS":12}}"#).unwrap_err().to_string().contains("API_TIMEOUT_MS"));
        assert!(
            parse_gateway_input(
                r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","BAD-NAME":"x"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn g2_drops_only_credential_segments() {
        // Known-bad: a substring filter drops MAX_OUTPUT_TOKENS or misses CUSTOM_HEADERS.
        let input = r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_AUTH_TOKEN":"TOKEN-CANARY","ANTHROPIC_API_KEY":"TOKEN-CANARY","ANTHROPIC_CUSTOM_HEADERS":"TOKEN-CANARY","ANTHROPIC_FOUNDRY_API_KEY":"TOKEN-CANARY","AWS_SESSION_TOKEN":"TOKEN-CANARY","CLAUDE_CODE_MAX_OUTPUT_TOKENS":"1","API_TIMEOUT_MS":"600000","CLAUDE_CODE_SKIP_BEDROCK_AUTH":"1"}}"#;
        let GatewayInput::Json {
            settings, dropped, ..
        } = parse_gateway_input(input).unwrap()
        else {
            panic!("expected JSON");
        };
        assert_eq!(
            dropped,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_CUSTOM_HEADERS",
                "ANTHROPIC_FOUNDRY_API_KEY",
                "AWS_SESSION_TOKEN"
            ]
        );
        for name in [
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "API_TIMEOUT_MS",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
        ] {
            assert!(settings.contains_key(name));
        }
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("TOKEN-CANARY")
        );
    }

    #[test]
    fn g3_url_rule_refuses_credentials_and_unsafe_transport() {
        // Known-bad: accepting userinfo or a query string leaks credentials through a URL.
        assert_eq!(
            normalize_base_url("HTTPS://Gateway.Example.Com/Path///").unwrap(),
            "https://gateway.example.com/Path"
        );
        assert_eq!(
            normalize_base_url("http://LOCALHOST:4000/a/").unwrap(),
            "http://localhost:4000/a"
        );
        for input in [
            "http://gateway.example.com",
            "https://user@gateway.example.com",
            "https://gateway.example.com/a?token=x",
            "https://gateway.example.com/a#frag",
            "https://gateway.example.com/a b",
            "https://",
        ] {
            assert!(normalize_base_url(input).is_err(), "unsafe URL accepted");
        }
        assert!(parse_gateway_input(" https://gateway.example.com ").is_err());
    }

    #[test]
    fn g12_defaults_are_private_normalized_and_never_overwrite_bad_input() {
        // Known-bad: keying defaults by raw URLs or overwriting an unparsable store.
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let url = normalize_base_url("HTTPS://Gateway.Example.Com/a/").unwrap();
        let settings = BTreeMap::from([
            ("ANTHROPIC_BASE_URL".into(), url.clone()),
            ("ANTHROPIC_MODEL".into(), "vendor/claude-model".into()),
        ]);
        let mut defaults = Defaults::new();
        defaults.insert(url.clone(), settings);
        write_defaults(base, &defaults).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(base.join("gateways.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(read_defaults(base).unwrap(), defaults);
        let listed = list(base).unwrap();
        assert!(listed.contains("ANTHROPIC_MODEL"));
        assert!(!listed.contains("vendor/claude-model"));
        assert!(listed.lines().all(|line| line.len() <= 120));
        assert_eq!(forget(base, "https://GATEWAY.EXAMPLE.COM/a/").unwrap(), url);
        assert!(read_defaults(base).unwrap().is_empty());
        fs::write(base.join("gateways.json"), r#"{"version":2,"gateways":{}}"#).unwrap();
        assert!(read_defaults(base).is_err());
        fs::write(base.join("gateways.json"), "bad").unwrap();
        assert!(read_defaults(base).is_err());
        assert!(forget(base, "https://gateway.example.com/a").is_err());
        assert_eq!(fs::read(base.join("gateways.json")).unwrap(), b"bad");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(base.join("gateways.json")).unwrap();
            symlink(base.join("missing"), base.join("gateways.json")).unwrap();
            assert!(read_defaults(base).is_err());
        }
    }
}
