// #479 golden fixture — TypeScript, parsed under a production vname path.

// None: ordinary production code.
export function calibrateFloors(): void {}

// Adversarial None: a test-ish name in a production (non-test-path) file.
export function testConnectionPool(): void {}
