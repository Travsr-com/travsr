// Travsr test fixture — small Rust file covering all node kinds extracted by
// the Phase A+B structural parser (INDEX-201, INDEX-202). Do NOT edit without
// updating the golden snapshot tests in crates/travsr-indexer/tests/rust.rs.
use std::fmt;
use std::{collections::HashMap, io};

// File-system module declaration (mod foo; without a body).
mod helpers;

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
