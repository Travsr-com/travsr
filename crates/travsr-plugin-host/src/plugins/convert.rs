use travsr_indexer::{FfiMarker as RichFfi, FfiMarkerKind as RichKind, ParseOutput};
use travsr_plugin_protocol::{FfiMarker as WireFfi, FfiMarkerKind as WireKind, ParseResponse};

pub fn parse_output_to_response(out: ParseOutput) -> ParseResponse {
    ParseResponse {
        nodes: out.nodes,
        edges: out.edges,
        ffi_markers: out
            .ffi_markers
            .into_iter()
            .filter_map(|m| {
                Some(WireFfi {
                    source_node_id: m.node_id.0,
                    kind: map_kind_to_wire(&m.kind)?,
                    local_name: m.local_name.clone(),
                    bound_name: m.bound_name.clone(),
                    arity: m.arity,
                    module: m.module.clone(),
                    corpus: m.corpus.clone(),
                })
            })
            .collect(),
    }
}

/// Reverse conversion: wire → ParseOutput. Used by PluginIndexer to return
/// a type the daemon can use unchanged. All FfiMarker fields are preserved
/// losslessly now that the wire format carries the full field set (P5-S4).
pub fn response_to_output(resp: ParseResponse) -> ParseOutput {
    ParseOutput {
        nodes: resp.nodes,
        edges: resp.edges,
        ffi_markers: resp
            .ffi_markers
            .into_iter()
            .filter_map(|m| {
                RichFfi::try_new(
                    travsr_core::NodeId(m.source_node_id),
                    map_kind_from_wire(&m.kind),
                    m.local_name,
                    m.bound_name,
                    m.arity,
                    m.module,
                    m.corpus,
                )
            })
            .collect(),
    }
}

fn map_kind_to_wire(k: &RichKind) -> Option<WireKind> {
    Some(match k {
        RichKind::NapiExport => WireKind::NapiExport,
        RichKind::NapiCall => WireKind::NapiCall,
        RichKind::PyO3Export => WireKind::PyO3Export,
        RichKind::PyO3Call => WireKind::PyO3Call,
        RichKind::CgoExport => WireKind::CgoExport,
        RichKind::GoCallC => WireKind::GoCallC,
        RichKind::JniExport => WireKind::JniExport,
        RichKind::JniCall => WireKind::JniCall,
    })
}

fn map_kind_from_wire(k: &WireKind) -> RichKind {
    match k {
        WireKind::NapiExport => RichKind::NapiExport,
        WireKind::NapiCall => RichKind::NapiCall,
        WireKind::PyO3Export => RichKind::PyO3Export,
        WireKind::PyO3Call => RichKind::PyO3Call,
        WireKind::CgoExport => RichKind::CgoExport,
        WireKind::GoCallC => RichKind::GoCallC,
        WireKind::JniExport => RichKind::JniExport,
        WireKind::JniCall => RichKind::JniCall,
    }
}
