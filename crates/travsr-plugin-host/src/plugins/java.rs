// Java Phase A parser now lives in travsr-analysis::java.
// This file is a thin Plugin wrapper that delegates to it.

use travsr_core::Language;
use travsr_plugin_protocol::{InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin};

use crate::plugins::parse_output_to_response;

pub struct JavaPlugin;

impl Plugin for JavaPlugin {
    fn language(&self) -> Language {
        Language::Java
    }

    fn extensions(&self) -> &[&str] {
        travsr_analysis::java::EXTENSIONS
    }

    fn supports_phase_b(&self) -> bool {
        false
    }

    fn parse(&self, req: &ParseRequest) -> ParseResponse {
        match travsr_analysis::java::parse(&req.corpus, &req.path, &req.vname_path) {
            Ok(out) => parse_output_to_response(out),
            Err(e) => {
                tracing::warn!(event = "file.parse_failed", lang = "java", path = %req.path.display(), err = %e, "parse error, skipping");
                ParseResponse::default()
            }
        }
    }

    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::default()
    }
}
