use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FfiMarker {
    pub source_node_id: u64,
    pub kind: FfiMarkerKind,
    pub local_name: String,
    pub bound_name: Option<String>,
    pub arity: Option<u8>,
    pub module: Option<String>,
    pub corpus: String,
}

impl FfiMarker {
    pub fn effective_name(&self) -> &str {
        self.bound_name.as_deref().unwrap_or(&self.local_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum FfiMarkerKind {
    #[default]
    NapiExport,
    NapiCall,
    PyO3Export,
    PyO3Call,
    CgoExport,
    GoCallC,
    JniExport,
    JniCall,
}
