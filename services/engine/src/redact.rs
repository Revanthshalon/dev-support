//! Masks secret-looking values in any text before it leaves the crate: tool output, errors, logs.

use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

/// Keys whose value is secret: the key ends in one of these words (so `tokens_est` and `max_tokens`
/// stay readable).
const KEY: &str = r"\b([A-Za-z0-9_.-]*?(?:password|passwd|secret|token|api[_-]?key|private[_-]?key|access[_-]?key(?:[_-]?id)?|secret[_-]?key|dsn|credentials?)";

/// `(pattern, replacement)`, applied in order. URL userinfo goes first so a DSN's password is masked
/// even when its key isn't secret-looking. Quoted values keep their quotes, so JSON stays JSON.
static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let sep = r#"["']?\s*[:=]\s*"#;
    [
        // scheme://user:PASS@host; greedy up to the last `@`, since raw `@` in passwords happens.
        // It stops at quotes and angle brackets, so it can't run across JSON strings or markup.
        (
            r#"(?i)\b([a-z][a-z0-9+.-]*://[^:/@\s"'<>]*:)[^\s/"'<>]+@"#.to_string(),
            "${1}***@",
        ),
        // Authorization: Bearer <token> / Basic <base64>
        (
            r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+".to_string(),
            "${1}***",
        ),
        (r"(?i)\b(basic\s+)[A-Za-z0-9+/=]{6,}".to_string(), "${1}***"),
        // provider tokens that show up without a key, by their real shape, so identifiers like
        // `hf_hub_download` survive: OpenAI sk-… (long), Hugging Face hf_ + 30+ alphanumerics
        (r"\bsk-[A-Za-z0-9_-]{20,}".to_string(), "sk-***"),
        (r"\bhf_[A-Za-z0-9]{30,}".to_string(), "hf_***"),
        // KEY: "value", KEY='value', KEY=value
        (format!(r#"(?i){KEY}{sep}")[^"]*""#), "${1}***\""),
        (format!(r#"(?i){KEY}{sep}')[^']*'"#), "${1}***'"),
        (format!(r#"(?i){KEY}{sep})[^"'\s,;&}}]+"#), "${1}***"),
    ]
    .into_iter()
    .map(|(re, rep)| (Regex::new(&re).expect("redact pattern"), rep))
    .collect()
});

/// Returns `text` with secret-looking values replaced by `***`.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (re, rep) in RULES.iter() {
        // Only allocate when a rule actually matched.
        if let Cow::Owned(s) = re.replace_all(&out, *rep) {
            out = s;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts() {
        let s = redact(
            "MEMGRAPH_PASSWORD=test postgres://u:hunter2@db/x \"api_key\": \"abc\" user=bob",
        );
        assert!(
            !s.contains("hunter2") && !s.contains("abc") && !s.contains("=test"),
            "{s}"
        );
        assert!(
            s.contains("user=bob") && s.contains("postgres://u:***@db/x"),
            "{s}"
        );
    }

    #[test]
    fn redacts_key_variants() {
        for (input, secret) in [
            ("DB_PASSWD: s3cr3t", "s3cr3t"),
            ("client_secret='topsecret'", "topsecret"),
            ("X_AUTH_TOKEN=tok123", "tok123"),
            ("private-key = pk999", "pk999"),
            ("SENTRY_DSN=https://k@sentry.io/1", "https://k"),
            ("aws_credential: cred42", "cred42"),
            ("Authorization: Bearer eyJhbGciOi.payload", "eyJhbGciOi"),
            (
                "token sk-proj-abcdef1234567890abcdef rejected",
                "abcdef123456",
            ),
            (
                "hf_AbCdEfGhIjKlMnOpQrStUvWxYz012345 is invalid",
                "AbCdEfGhIjKlMnOp",
            ),
            ("redis://:pw-only@cache:6379/0", "pw-only"),
        ] {
            let out = redact(input);
            assert!(!out.contains(secret), "{input} -> {out}");
        }
    }

    #[test]
    fn redacts_review_findings() {
        for (input, secret) in [
            ("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI", "wJalrXUtnFEMI"),
            ("SECRET_KEY=django123", "django123"),
            ("secret_access_key: aks12345", "aks12345"),
            ("AWS_ACCESS_KEY_ID=AKIAEXAMPLE", "AKIAEXAMPLE"),
            ("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("password = \"hunter 2\"", "2"),
            ("MEMGRAPH_PASSWORD: \"a,b\"", ",b"),
            ("postgres://u:p@ss@host/db", "ss"),
        ] {
            let out = redact(input);
            assert!(!out.contains(secret), "{input} -> {out}");
        }
        assert_eq!(
            redact("postgres://u:p@ss@host/db"),
            "postgres://u:***@host/db"
        );
    }

    #[test]
    fn url_rule_stays_inside_json_strings() {
        let s = r#"{"a":"redis://:pw@cache:6379","b":"x@y.com"}"#;
        assert_eq!(
            redact(s),
            r#"{"a":"redis://:***@cache:6379","b":"x@y.com"}"#
        );
    }

    #[test]
    fn identifiers_that_look_like_token_prefixes_survive() {
        for s in [
            "calling hf_hub_download",
            "use sk-learn-style naming",
            "task-runner sk-1",
        ] {
            assert_eq!(redact(s), s);
        }
        assert_eq!(redact("hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123456"), "hf_***");
        assert_eq!(redact("sk-proj-abcdefghij0123456789"), "sk-***");
    }

    #[test]
    fn keeps_token_counts() {
        for s in [
            "max_tokens: 256",
            "overlap_tokens=32 max_chunks_per_symbol: 32",
            "remote_max_tokens: 512",
        ] {
            assert_eq!(redact(s), s);
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for s in [
            "payment failed for order 42",
            "connect to postgres://db:5432/orders failed",
            "keyspace hits: 10, tokens_est=12",
            "user=bob host=db",
        ] {
            assert_eq!(redact(s), s);
        }
    }
}
