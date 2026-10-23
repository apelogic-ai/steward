//! Shared process-log contract for Steward components.

use std::env;
use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;

use serde::Serialize;

const LOG_FORMAT_ENVIRONMENT: &str = "STEWARD_LOG_FORMAT";
const REDACTED: &str = "[redacted]";

#[derive(Clone, Copy)]
struct ProcessLogConfig {
    format: LogFormat,
    component: &'static str,
}

static PROCESS_LOG_CONFIG: OnceLock<ProcessLogConfig> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidLogFormat;

impl fmt::Display for InvalidLogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("STEWARD_LOG_FORMAT must be plain or json")
    }
}

impl std::error::Error for InvalidLogFormat {}

impl LogFormat {
    pub fn parse(value: Option<&str>) -> Result<Self, InvalidLogFormat> {
        match value {
            None | Some("plain") => Ok(Self::Plain),
            Some("json") => Ok(Self::Json),
            Some(_) => Err(InvalidLogFormat),
        }
    }
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Serialize)]
struct JsonLogEvent<'a> {
    level: &'static str,
    component: &'a str,
    request_id: Option<&'a str>,
    message: &'a str,
}

/// Initializes the process-wide log format and component name.
///
/// Invalid settings fall back to plain text and produce a fixed warning. The
/// rejected value is never copied into logs, and logging cannot prevent the
/// component from starting.
pub fn init(component: &'static str) {
    let configured = env::var(LOG_FORMAT_ENVIRONMENT);
    let parsed = configured
        .as_deref()
        .map(Some)
        .unwrap_or_else(|error| match error {
            env::VarError::NotPresent => None,
            env::VarError::NotUnicode(_) => Some(""),
        });
    let (format, invalid) = match LogFormat::parse(parsed) {
        Ok(format) => (format, false),
        Err(_) => (LogFormat::Plain, true),
    };
    let _ = PROCESS_LOG_CONFIG.set(ProcessLogConfig { format, component });
    if invalid {
        emit(
            LogLevel::Warn,
            None,
            format_args!("invalid log format; using plain text"),
        );
    }
}

pub fn emit(level: LogLevel, request_id: Option<&str>, message: fmt::Arguments<'_>) {
    let fallback = ProcessLogConfig {
        format: LogFormat::Plain,
        component: "steward",
    };
    let config = PROCESS_LOG_CONFIG.get().copied().unwrap_or(fallback);
    let message = message.to_string();
    let mut stderr = io::stderr().lock();
    let _ = write_event(
        &mut stderr,
        config.format,
        level,
        config.component,
        request_id,
        &message,
    );
}

pub fn emit_with_request_id(
    level: LogLevel,
    request_id: &impl fmt::Display,
    message: fmt::Arguments<'_>,
) {
    let request_id = request_id.to_string();
    emit(level, Some(&request_id), message);
}

#[macro_export]
macro_rules! event {
    ($level:expr, request_id = $request_id:expr, $($argument:tt)+) => {
        $crate::emit_with_request_id($level, &$request_id, format_args!($($argument)+))
    };
    ($level:expr, $($argument:tt)+) => {
        $crate::emit($level, None, format_args!($($argument)+))
    };
}

#[macro_export]
macro_rules! info {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Info, $($argument)+)
    };
}

#[macro_export]
macro_rules! warn {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Warn, $($argument)+)
    };
}

#[macro_export]
macro_rules! error {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Error, $($argument)+)
    };
}

pub fn render_event(
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    message: &str,
) -> String {
    let message = redact(message);
    let request_id = request_id.map(redact);
    match format {
        LogFormat::Plain => message,
        LogFormat::Json => {
            let event = JsonLogEvent {
                level: level.as_str(),
                component,
                request_id: request_id.as_deref(),
                message: &message,
            };
            serde_json::to_string(&event).unwrap_or_else(|_| {
                "{\"level\":\"error\",\"component\":\"steward\",\"request_id\":null,\"message\":\"log serialization failed\"}".to_owned()
            })
        }
    }
}

pub fn write_event(
    writer: &mut impl Write,
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    message: &str,
) -> io::Result<()> {
    writer.write_all(render_event(format, level, component, request_id, message).as_bytes())?;
    writer.write_all(b"\n")
}

fn redact(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut redact_following = 0_u8;
    for piece in message.split_inclusive(char::is_whitespace) {
        let token = piece.trim_end_matches(char::is_whitespace);
        let whitespace = &piece[token.len()..];
        let (replacement, following) = if redact_following > 0 {
            redact_following -= 1;
            (Some(REDACTED), 0)
        } else if let Some(following) = sensitive_field(token) {
            (Some(REDACTED), following)
        } else if token_is_bearer(token) {
            (Some(REDACTED), 1)
        } else if contains_secret_key(token)
            || is_jwt(token)
            || is_password_url(token)
            || is_email(token)
        {
            (Some(REDACTED), 0)
        } else {
            (None, 0)
        };
        redact_following = redact_following.max(following);
        output.push_str(replacement.unwrap_or(token));
        output.push_str(whitespace);
    }
    output
}

fn sensitive_field(token: &str) -> Option<u8> {
    let normalized = token.trim_start_matches(['"', '\'', '{', '[', '(']);
    let separator = normalized.find([':', '='])?;
    let field = normalized[..separator]
        .trim_matches(['"', '\'', '}', ']', ')'])
        .to_ascii_lowercase();
    let sensitive = matches!(
        field.as_str(),
        "authorization"
            | "proxy-authorization"
            | "token"
            | "access_token"
            | "access-token"
            | "api_key"
            | "api-key"
            | "key"
            | "secret"
            | "password"
            | "client_secret"
            | "client-secret"
            | "private_key"
            | "private-key"
            | "database_url"
            | "database-url"
    );
    if !sensitive {
        return None;
    }
    let value =
        normalized[separator + 1..].trim_matches(['"', '\'', '{', '}', '[', ']', '(', ')', ',']);
    if value.is_empty() {
        Some(if field.contains("authorization") {
            2
        } else {
            1
        })
    } else {
        Some(0)
    }
}

fn token_is_bearer(token: &str) -> bool {
    token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .eq_ignore_ascii_case("bearer")
}

fn contains_secret_key(token: &str) -> bool {
    let normalized = token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .to_ascii_lowercase();
    normalized
        .find("sk-")
        .is_some_and(|index| normalized.len().saturating_sub(index) >= 8)
}

fn is_jwt(token: &str) -> bool {
    let normalized = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
    });
    let mut segments = normalized.split('.');
    let valid_segment = |segment: &str| {
        segment.len() >= 8
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(first), Some(second), Some(third), None)
            if valid_segment(first) && valid_segment(second) && valid_segment(third)
    )
}

fn is_password_url(token: &str) -> bool {
    let normalized = token.trim_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
        )
    });
    let Some((_, rest)) = normalized.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let Some((credentials, _)) = authority.rsplit_once('@') else {
        return false;
    };
    credentials
        .split_once(':')
        .is_some_and(|(user, password)| !user.is_empty() && !password.is_empty())
}

fn is_email(token: &str) -> bool {
    let normalized = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-' | '+' | '@')
    });
    let Some((local, host)) = normalized.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && host.contains('.')
        && !host.ends_with('.')
        && !host.contains('@')
        && normalized.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'@')
        })
}

#[cfg(test)]
mod tests {
    use super::{InvalidLogFormat, LogFormat, LogLevel, render_event, write_event};

    #[test]
    fn plain_text_remains_the_default() -> Result<(), String> {
        if LogFormat::parse(None).map_err(|error| error.to_string())? != LogFormat::Plain {
            return Err("an absent setting must retain plain text logging".to_owned());
        }
        let rendered = render_event(
            LogFormat::Plain,
            LogLevel::Info,
            "steward-apiserver",
            None,
            "service started",
        );
        if rendered != "service started" {
            return Err("plain text must preserve a safe message".to_owned());
        }
        Ok(())
    }

    #[test]
    fn json_is_exactly_one_object_with_stable_fields() -> Result<(), String> {
        let mut output = Vec::new();
        write_event(
            &mut output,
            LogFormat::Json,
            LogLevel::Error,
            "steward-controller",
            Some("request-123"),
            "first line\nsecond line",
        )
        .map_err(|error| error.to_string())?;
        let output = String::from_utf8(output).map_err(|error| error.to_string())?;
        if output.lines().count() != 1 {
            return Err("one event must occupy exactly one line".to_owned());
        }
        let value: serde_json::Value =
            serde_json::from_str(output.trim_end()).map_err(|error| error.to_string())?;
        let expected = serde_json::json!({
            "level": "error",
            "component": "steward-controller",
            "request_id": "request-123",
            "message": "first line\nsecond line",
        });
        if value != expected {
            return Err(format!("unexpected JSON log contract: {value}"));
        }
        Ok(())
    }

    #[test]
    fn absent_request_id_is_json_null() -> Result<(), String> {
        let rendered = render_event(
            LogFormat::Json,
            LogLevel::Warn,
            "steward-mint",
            None,
            "credential lookup failed",
        );
        let value: serde_json::Value =
            serde_json::from_str(&rendered).map_err(|error| error.to_string())?;
        if value.get("request_id") != Some(&serde_json::Value::Null) {
            return Err("an absent request id must be JSON null".to_owned());
        }
        Ok(())
    }

    #[test]
    fn sensitive_values_are_redacted_without_dropping_the_event() -> Result<(), String> {
        let password_url = [
            "database postgresql://alice:",
            "password-fixture",
            "@db.example.test/steward failed",
        ]
        .concat();
        let cases = [
            (
                "Authorization: Bearer auth-fixture-value",
                "auth-fixture-value",
            ),
            (
                "request failed with Bearer bearer-fixture-value",
                "bearer-fixture-value",
            ),
            (
                "provider returned sk-fixture-token-value",
                "sk-fixture-token-value",
            ),
            (
                "assertion eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
                "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
            ),
            (password_url.as_str(), "password-fixture"),
            ("user alice@example.com was rejected", "alice@example.com"),
            ("api_key=key-fixture-value", "key-fixture-value"),
        ];
        for format in [LogFormat::Plain, LogFormat::Json] {
            for (message, forbidden) in cases {
                let rendered =
                    render_event(format, LogLevel::Error, "steward-apiserver", None, message);
                if rendered.contains(forbidden) {
                    return Err(format!(
                        "sensitive value remained in log output: {forbidden}"
                    ));
                }
                if !rendered.contains("[redacted]") {
                    return Err("redaction must retain the event with a fixed marker".to_owned());
                }
            }
        }
        let rendered = render_event(
            LogFormat::Json,
            LogLevel::Error,
            "steward-controller",
            Some("Bearer request-id-fixture"),
            "request failed",
        );
        if rendered.contains("request-id-fixture") || !rendered.contains("[redacted]") {
            return Err("request identifiers must use the same redaction contract".to_owned());
        }
        Ok(())
    }

    #[test]
    fn invalid_format_error_never_echoes_the_value() -> Result<(), String> {
        let Err(InvalidLogFormat) = LogFormat::parse(Some("json-with-secret-fixture")) else {
            return Err("an unsupported format must be refused".to_owned());
        };
        let message = InvalidLogFormat.to_string();
        if message.contains("secret-fixture") {
            return Err("configuration errors must not echo rejected input".to_owned());
        }
        Ok(())
    }
}
