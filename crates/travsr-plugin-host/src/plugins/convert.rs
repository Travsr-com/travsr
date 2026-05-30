use travsr_indexer::{ParseOutput, FfiMarker as RichFfi, FfiMarkerKind as RichKind};
use travsr_plugin_protocol::{
    ParseResponse,
    FfiMarker as WireFfi,
    FfiMarkerKind as WireKind,
};

pub fn parse_output_to_response(out: ParseOutput) -> ParseResponse {
    ParseResponse {
        nodes: out.nodes,
        edges: out.edges,
        ffi_markers: out.ffi_markers.into_iter()
            .map(|m| WireFfi {
                source_node_id: m.node_id.0,
                target_symbol: m.effective_name().to_string(),
                kind: map_kind_to_wire(&m.kind),
            })
            .collect(),
    }
}

/// Reverse conversion: wire → ParseOutput. Used by PluginIndexer to return
/// a type the daemon can use unchanged. FfiMarker fields not in the wire
/// format (arity, module, bound_name) are zeroed — P5-S4 enriches the wire.
pub fn response_to_output(resp: ParseResponse) -> ParseOutput {
    ParseOutput {
        nodes: resp.nodes,
        edges: resp.edges,
        ffi_markers: resp.ffi_markers.into_iter()
            .filter_map(|m| {
                // effective_name() == local_name when bound_name is None.
                let kind = map_kind_from_wire(&m.kind);
                RichFfi::try_new(
                    travsr_core::NodeId(m.source_node_id),
                    kind,
                    m.target_symbol,
                    None::<String>,
                    None,
                    None::<String>,
                    "",
                ).or_else(|| {
                    // try_new rejects invalid names; create a sentinel marker
                    // with a safe placeholder name that the resolver will skip.
                    RichFfi::try_new(
                        travsr_core::NodeId(0),
                        RichKind::NapiExport,
                        "unknown",
                        None::<String>,
                        None,
                        None::<String>,
                        "",
                    )
                })
            })
            .collect(),
    }
}

fn map_kind_to_wire(k: &RichKind) -> WireKind {
    match k {
        RichKind::NapiExport | RichKind::NapiCall |
        RichKind::PyO3Export | RichKind::PyO3Call |
        RichKind::CgoExport  | RichKind::GoCallC  => WireKind::CCall,
    }
}

fn map_kind_from_wire(k: &WireKind) -> RichKind {
    match k {
        WireKind::CCall | WireKind::WasmImport => RichKind::NapiExport,
        WireKind::JniCall => RichKind::NapiExport, // closest available in P5-S2
    }
}
