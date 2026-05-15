use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};

pub fn file_node(path: &str) -> Node {
    Node::new(ts_vname(path, "file"), "file")
}

pub fn class_node(path: &str, class_name: &str) -> Node {
    Node::new(ts_vname(path, &format!("class:{class_name}")), "class")
}

pub fn fn_node(path: &str, fn_name: &str) -> Node {
    Node::new(ts_vname(path, &format!("fn:{fn_name}")), "function")
}

pub fn method_node(path: &str, class_name: &str, method_name: &str) -> Node {
    Node::new(
        ts_vname(path, &format!("method:{class_name}.{method_name}")),
        "method",
    )
}

pub fn var_node(path: &str, var_name: &str) -> Node {
    Node::new(ts_vname(path, &format!("var:{var_name}")), "variable")
}

pub fn import_node(path: &str, module: &str) -> Node {
    Node::new(ts_vname(path, &format!("import:{module}")), "import")
}

fn ts_vname(path: &str, signature: &str) -> VName {
    VName::new("", "", path, "typescript", signature)
}

pub fn defines_edge(src: NodeId, dst: NodeId) -> Edge {
    Edge::new(src, dst, EdgeKind::DefinesBinding)
}

pub fn depends_edge(src: NodeId, dst: NodeId) -> Edge {
    Edge::new(src, dst, EdgeKind::Depends)
}
