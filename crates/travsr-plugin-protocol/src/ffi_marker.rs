use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FfiMarker {
    pub source_node_id: u64,
    pub target_symbol: String,
    pub kind: FfiMarkerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum FfiMarkerKind {
    #[default]
    CCall,
    JniCall,
    WasmImport,
}
