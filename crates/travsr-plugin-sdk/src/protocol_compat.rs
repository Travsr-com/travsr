use travsr_plugin_protocol::PluginError;

/// Refuse a daemon whose wire version is not exactly the one this plugin was
/// compiled against.
///
/// The protocol version is a monotonic integer, not a semver, so there is no
/// "compatible major" to accept: RFC-011 §4 and ADR-017 make any difference
/// fail-fast on both sides, and the host applies the same `!=` to the response.
pub(crate) fn ensure_protocol_compatible(
    daemon_version: u32,
    supported_version: u32,
) -> Result<(), PluginError> {
    if daemon_version == supported_version {
        return Ok(());
    }
    Err(PluginError {
        file: String::new(),
        message: format!(
            "incompatible daemon protocol version: daemon speaks {daemon_version}, \
             plugin speaks {supported_version}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_versions_are_compatible() {
        assert!(ensure_protocol_compatible(1, 1).is_ok());
    }

    #[test]
    fn unequal_versions_name_both_sides() {
        let err = ensure_protocol_compatible(2, 1).expect_err("2 != 1 must be refused");
        assert!(
            err.message.contains("daemon speaks 2") && err.message.contains("plugin speaks 1"),
            "unexpected message: {}",
            err.message
        );
    }
}
