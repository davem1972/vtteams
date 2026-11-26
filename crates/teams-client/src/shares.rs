//! Microsoft Graph's Shares API: resolving a SharePoint/OneDrive URL to a direct, pre-authenticated download.
//!
//! Teams uploads a pasted screenshot or a shared file to the sender's OneDrive rather than embedding it inline
//! (`docs/teams-web-protocol.md`), so its message carries a SharePoint-hosted URL, not a Teams-hosted one. Neither
//! backend's own bearer token is accepted there; Graph's `/shares/{id}/driveItem` resolves such a URL to metadata
//! that includes `@microsoft.graph.downloadUrl`, a short-lived URL that is itself already authenticated and needs
//! no further credential (observed by both `m0nkmaster/msteams-mcp` and `YourSandwich/mautrix-teams`, credited in
//! the top-level README; no code from either is copied here, and nothing from the AGPL-licensed mautrix-teams
//! ever should be). Shared by `teams::mod` (its own Graph-scoped token, requested only when a message needs it)
//! and `graph::mod` (which already holds one), so each only has to ask for the URL to fetch, not how to get there.

use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// Whether `url` is a SharePoint or OneDrive host (`<tenant>.sharepoint.com` for a site, `<tenant>-my.sharepoint.com`
/// for a personal OneDrive), where a shared file's own link lives rather than a Teams- or Graph-hosted one.
pub(crate) fn is_sharepoint_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| h.ends_with(".sharepoint.com"))
    })
}

#[cfg(test)]
static TEST_DOWNLOAD_ORIGINS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Let tests treat a local server as somewhere a resolved download may come from. Each test server has its own port,
/// so entries never collide; a server a test does not register stays refused, which is how the refusals are tested.
#[cfg(test)]
pub(crate) fn allow_download_origin_for_tests(origin: &str) {
    TEST_DOWNLOAD_ORIGINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(origin.to_string());
}

/// Whether a pre-authenticated download URL, which Graph chose and nothing of ours has checked, may be fetched: `https`,
/// with a host that is a DNS name (not an IP address of any spelling, not `localhost` or another name that only
/// means this computer or a private network), no user name or password in it, and the scheme's own port. Nothing of
/// ours is sent to it, but the request itself would otherwise be a way to make this computer ask for anything on its
/// own network. Applied to the first URL and to every redirect hop. There is deliberately no list of allowed host
/// names: where Microsoft serves these from is not verified, and a wrong list would break working pictures.
pub(crate) fn is_download_url(url: &str) -> bool {
    #[cfg(test)]
    if TEST_DOWNLOAD_ORIGINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|o| url.starts_with(o.as_str()))
    {
        return true;
    }
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
    {
        return false;
    }
    match u.host() {
        Some(url::Host::Domain(name)) => is_public_dns_name(name),
        _ => false,
    }
}

/// A name that could be a public host: dotted labels of letters, digits, hyphens and underscores, none empty, and
/// not one of the names that mean this computer or a private network.
fn is_public_dns_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if name.is_empty() || name.len() > 253 || !name.contains('.') {
        return false;
    }
    const PRIVATE_SUFFIXES: [&str; 5] =
        ["localhost", "local", "internal", "localdomain", "home.arpa"];
    if PRIVATE_SUFFIXES
        .iter()
        .any(|s| name == *s || name.ends_with(&format!(".{s}")))
    {
        return false;
    }
    let labels_ok = name.split('.').all(|label| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    // A last label of digits only is an address spelt oddly, never a name.
    labels_ok
        && !name
            .rsplit('.')
            .next()
            .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `url` is Microsoft Graph itself. Used by a caller that holds a Graph-scoped token only for resolving a
/// shared file (the Teams web backend), to check before sending it anywhere; the Graph backend holds that token
/// for everything it does and makes the same check against its own base URL already.
#[cfg(feature = "teams")]
pub(crate) fn is_graph_url(url: &str) -> bool {
    url::Url::parse(url)
        .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("graph.microsoft.com"))
}

/// Microsoft Graph's own base URL in production. Only the Teams web backend's `graph_base` field defaults to
/// this; the Graph backend already has its own base URL (`graph::BASE`) for everything it does, this call
/// included.
#[cfg(feature = "teams")]
pub(crate) const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// The Graph endpoint that resolves a shared item (a direct file link or a sharing link alike) to its metadata,
/// including a pre-authenticated download URL. `url` is encoded the way Graph's Shares API documents: `u!` plus
/// unpadded URL-safe base64 of the UTF-8 bytes of the URL.
pub(crate) fn shared_driveitem_url(base: &str, url: &str) -> String {
    format!(
        "{base}/shares/u!{}/driveItem",
        URL_SAFE_NO_PAD.encode(url.as_bytes())
    )
}

/// Pull the pre-authenticated download URL out of a resolved driveItem answer.
pub(crate) fn download_url(v: &Value) -> Result<String> {
    const OPERATION: &str = "resolve shared file";
    v["@microsoft.graph.downloadUrl"]
        .as_str()
        .map(String::from)
        .ok_or(Error::Unexpected {
            operation: OPERATION,
            detail: "no download URL in the answer",
        })
}

/// Extract the tenant ID from a resolved driveItem's `sharepointIds.tenantId` field, when present.
/// Returns `None` if the field is missing or not a string (not an error: a missing tenant ID means the content
/// cannot be verified and should not be auto-loaded).
pub(crate) fn tenant_id(v: &Value) -> Option<String> {
    v["sharepointIds"]["tenantId"].as_str().map(String::from)
}

/// Compare two tenant IDs case-insensitively and with leading/trailing whitespace trimmed. GUIDs are case-insensitive
/// and may come from different sources with different casing.
pub(crate) fn same_tenant(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        _ => false,
    }
}

/// Download URLs the backends' tests must see refused, whatever the first server says: `elsewhere` is a plain http
/// loopback server the test has not allowed. None of the others can be reached from a test: they are refused before
/// any lookup or connection, and the host names that look real are in a reserved, unresolvable zone.
#[cfg(test)]
pub(crate) fn refused_download_targets(elsewhere: &str) -> Vec<String> {
    vec![
        format!("{elsewhere}/dl"),
        "http://dl.invalid/x".into(),
        "https://127.0.0.1/x".into(),
        "https://[::1]/x".into(),
        "https://2130706433/x".into(),
        "https://localhost/x".into(),
        "https://user:pw@dl.invalid/x".into(),
        "https://dl.invalid:8443/x".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sharepoint_and_onedrive_hosts_are_recognised() {
        for url in [
            "https://contoso.sharepoint.com/sites/x/doc.docx",
            "https://contoso-my.sharepoint.com/personal/bob/doc.docx",
        ] {
            assert!(is_sharepoint_url(url), "{url}");
        }
        for url in [
            "http://contoso.sharepoint.com/x",
            "https://evil.example/contoso.sharepoint.com",
            "https://graph.microsoft.com/v1.0/x",
            "not a url",
        ] {
            assert!(!is_sharepoint_url(url), "{url}");
        }
    }

    #[test]
    fn only_a_public_https_name_is_a_download() {
        for url in [
            "https://contoso-my.sharepoint.com/personal/bob/_layouts/15/download.aspx?UniqueId=1&tempauth=2",
            "https://cdn.example.net/a.png",
            "https://xn--bcher-kva.example/a.png",
            "https://host.example:443/a.png",
            "https://a_b.example/x",
        ] {
            assert!(is_download_url(url), "{url}");
        }
        for url in refused_download_targets("http://127.0.0.1:9") {
            assert!(!is_download_url(&url), "{url}");
        }
        for url in [
            "https://127.1/x",
            "https://0x7f.0.0.1/x",
            "https://10.0.0.1/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[fe80::1]/x",
            "https://LOCALHOST/x",
            "https://localhost./x",
            "https://app.localhost/x",
            "https://printer.local/x",
            "https://db.internal/x",
            "https://intranet/x",
            "https://user@dl.invalid/x",
            "https://:pw@dl.invalid/x",
            "https://dl.invalid:444/x",
            "https://dl.invalid:80/x",
            "ftp://dl.invalid/x",
            "file:///etc/passwd",
            "//dl.invalid/x",
            "not a url",
            "",
        ] {
            assert!(!is_download_url(url), "{url}");
        }
    }

    #[test]
    #[cfg(feature = "teams")]
    fn only_graph_itself_is_a_graph_url() {
        assert!(is_graph_url("https://graph.microsoft.com/v1.0/shares/x"));
        for url in [
            "http://graph.microsoft.com/v1.0/x",
            "https://graph.microsoft.com.evil.example/x",
            "https://notgraph.microsoft.com/x",
        ] {
            assert!(!is_graph_url(url), "{url}");
        }
    }

    #[test]
    fn the_share_id_is_graphs_own_encoding() {
        // Graph's documented example: https://support.microsoft.com -> u!aHR0cHM6Ly9zdXBwb3J0Lm1pY3Jvc29mdC5jb20
        assert_eq!(
            shared_driveitem_url(
                "https://graph.microsoft.com/v1.0",
                "https://support.microsoft.com"
            ),
            "https://graph.microsoft.com/v1.0/shares/u!aHR0cHM6Ly9zdXBwb3J0Lm1pY3Jvc29mdC5jb20/driveItem"
        );
        // A caller's own base (a local test server, say) is used verbatim.
        assert_eq!(
            shared_driveitem_url("http://127.0.0.1:1234", "https://x"),
            "http://127.0.0.1:1234/shares/u!aHR0cHM6Ly94/driveItem"
        );
    }

    #[test]
    fn the_download_url_is_read_from_its_own_field() {
        let v = json!({ "@microsoft.graph.downloadUrl": "https://x/y", "name": "a.png" });
        assert_eq!(download_url(&v).unwrap(), "https://x/y");
        assert!(matches!(
            download_url(&json!({ "name": "a.png" })),
            Err(Error::Unexpected { .. })
        ));
    }

    #[test]
    fn tenant_id_is_read_from_sharepoint_ids_when_present() {
        let v = json!({
            "@microsoft.graph.downloadUrl": "https://x/y",
            "sharepointIds": { "tenantId": "abc-123" }
        });
        assert_eq!(tenant_id(&v), Some("abc-123".to_string()));
        // Missing field returns None, not an error.
        assert_eq!(tenant_id(&json!({ "name": "a.png" })), None);
        assert_eq!(
            tenant_id(&json!({ "sharepointIds": { "siteId": "x" } })),
            None
        );
        // Non-string value also returns None.
        assert_eq!(
            tenant_id(&json!({ "sharepointIds": { "tenantId": 123 } })),
            None
        );
    }

    #[test]
    fn tenant_ids_are_compared_case_insensitively_and_trimmed() {
        // GUIDs are case-insensitive.
        assert!(same_tenant(Some("abc-123"), Some("ABC-123")));
        assert!(same_tenant(
            Some("12345678-ABCD-1234-abcd-123456789012"),
            Some("12345678-abcd-1234-ABCD-123456789012")
        ));
        // Whitespace is trimmed.
        assert!(same_tenant(Some(" abc-123 "), Some("abc-123")));
        // Different tenants do not match.
        assert!(!same_tenant(Some("abc-123"), Some("def-456")));
        // Missing tenant on either side does not match.
        assert!(!same_tenant(None, Some("abc-123")));
        assert!(!same_tenant(Some("abc-123"), None));
        assert!(!same_tenant(None, None));
    }
}
