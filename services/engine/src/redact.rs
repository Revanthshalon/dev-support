//! Masks secret-looking values in any text before it leaves the crate: tool output, errors, logs.

use regex::Regex;
use std::sync::LazyLock;

/// `(pattern, replacement)`, applied in order. URL userinfo goes first so a DSN's password is masked
/// even when its key isn't secret-looking.
static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        // scheme://user:PASS@host
        (r"(?i)\b([a-z][a-z0-9+.-]*://[^:/@\s]*:)[^@\s/]+@", "${1}***@"),
        // Authorization: Bearer <token>
        (r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+", "${1}***"),
        // provider tokens that show up without a key: OpenAI sk-…, Hugging Face hf_…
        (r"\b(sk-|hf_)[A-Za-z0-9_-]{6,}", "${1}***"),
        // KEY=value / "key": "value" where the key ends in a secret-looking word
        (
            r#"(?i)\b([A-Za-z0-9_.-]*?(?:password|passwd|secret|token|api[_-]?key|private[_-]?key|dsn|credential)s?["']?\s*[:=]\s*["']?)[^"'\s,;&}]+"#,
            "${1}***",
        ),
    ]
    .into_iter()
    .map(|(re, rep)| (Regex::new(re).expect("redact pattern"), rep))
    .collect()
});

/// Returns `text` with secret-looking values replaced by `***`.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (re, rep) in RULES.iter() {
        out = re.replace_all(&out, *rep).into_owned();
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
            ("token sk-proj-abcdef123456 rejected", "abcdef123456"),
            ("hf_AbCdEfGhIjKlMnOp is invalid", "AbCdEfGhIjKlMnOp"),
            ("redis://:pw-only@cache:6379/0", "pw-only"),
        ] {
            let out = redact(input);
            assert!(!out.contains(secret), "{input} -> {out}");
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
