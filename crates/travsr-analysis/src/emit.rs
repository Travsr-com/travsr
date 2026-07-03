use travsr_core::{Edge, EdgeKind, Node, NodeId, VName};

pub fn file_node(corpus: &str, path: &str) -> Node {
    Node::new(ts_vname(corpus, path, "file"), "file")
}

pub fn class_node(corpus: &str, path: &str, class_name: &str) -> Node {
    Node::new(
        ts_vname(corpus, path, &format!("class:{class_name}")),
        "class",
    )
}

pub fn fn_node(corpus: &str, path: &str, fn_name: &str) -> Node {
    Node::new(ts_vname(corpus, path, &format!("fn:{fn_name}")), "function")
}

pub fn method_node(corpus: &str, path: &str, class_name: &str, method_name: &str) -> Node {
    Node::new(
        ts_vname(corpus, path, &format!("method:{class_name}.{method_name}")),
        "method",
    )
}

pub fn interface_node(corpus: &str, path: &str, iface_name: &str) -> Node {
    Node::new(
        ts_vname(corpus, path, &format!("interface:{iface_name}")),
        "interface",
    )
}

pub fn type_node(corpus: &str, path: &str, type_name: &str) -> Node {
    Node::new(ts_vname(corpus, path, &format!("type:{type_name}")), "type")
}

pub fn enum_node(corpus: &str, path: &str, enum_name: &str) -> Node {
    Node::new(ts_vname(corpus, path, &format!("enum:{enum_name}")), "enum")
}

pub fn var_node(corpus: &str, path: &str, var_name: &str) -> Node {
    Node::new(
        ts_vname(corpus, path, &format!("var:{var_name}")),
        "variable",
    )
}

pub fn import_node(corpus: &str, path: &str, module: &str) -> Node {
    Node::new(
        ts_vname(corpus, path, &format!("import:{module}")),
        "import",
    )
}

fn ts_vname(corpus: &str, path: &str, signature: &str) -> VName {
    VName::new(corpus, "", path, "typescript", signature)
}

pub fn defines_edge(src: NodeId, dst: NodeId) -> Edge {
    Edge::new(src, dst, EdgeKind::DefinesBinding)
}

pub fn depends_edge(src: NodeId, dst: NodeId) -> Edge {
    Edge::new(src, dst, EdgeKind::Depends)
}

pub fn resolves_to_edge(src: NodeId, dst: NodeId) -> Edge {
    Edge::new(src, dst, EdgeKind::ResolvesTo)
}
