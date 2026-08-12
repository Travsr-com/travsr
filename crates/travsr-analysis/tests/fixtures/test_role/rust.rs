// #479 golden fixture — Rust test-role classification.
//
// Four cases (see tests/test_role.rs):
//   1. real test entry point            -> EntryPoint
//   2. helper inside the test scope      -> Support
//   3. ordinary production code          -> None
//   4. production code with a test-ish   -> None   (adversarial: name only,
//      name (`test_*`, `TestRunner`)               no attribute / not in scope)

pub fn calibrate_semantic_floors() {}

// Adversarial: a production function whose name looks test-ish but carries no
// `#[test]` attribute and lives outside any `#[cfg(test)]` scope.
pub fn test_connection_pool() {}

// Adversarial: a production type whose name looks test-ish.
pub struct TestRunner;

#[cfg(test)]
mod tests {
    use super::*;

    // Support: a helper inside the test scope, no attribute of its own.
    fn helper() {}

    #[test]
    fn calibrate_works() {
        helper();
    }

    #[tokio::test]
    async fn async_calibrate() {}
}
