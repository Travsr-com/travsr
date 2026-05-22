// Travsr test fixture — small Rust file covering all node kinds extracted by
// the Phase A structural parser (INDEX-201). Do NOT edit without updating the
// golden snapshot tests in crates/travsr-indexer/src/rust.rs.
use std::fmt;

pub mod utils {
    pub fn helper() -> u32 {
        42
    }
}

pub struct Config {
    pub name: String,
}

pub enum Status {
    Active,
    Inactive,
}

pub trait Processor {
    fn process(&self) -> bool;
}

pub struct Worker;

impl Worker {
    pub fn new() -> Self {
        Worker
    }
}

impl Processor for Worker {
    fn process(&self) -> bool {
        true
    }
}

pub fn run(_config: Config) -> Status {
    Status::Active
}

pub const MAX_RETRIES: u32 = 3;

pub static APP_NAME: &str = "travsr";

// Unused import suppression — fmt::Display is referenced to keep `use std::fmt` valid.
impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}
