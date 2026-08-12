//! Issue #479 §7.1/§7.2 — per-language golden fixtures + the compile-time
//! coverage gate.
//!
//! Every fixture asserts the four cases from the issue:
//!   1. a real test entry point                    -> `EntryPoint`
//!   2. a helper inside the test scope             -> `Support`
//!   3. ordinary production code                   -> `None`
//!   4. production code with a **test-ish name**   -> `None`   (adversarial)
//!
//! Case 4 is the robustness row: it fails any name-only rule that forgot its
//! corroborating signal (§2 asymmetric-cost invariant).
//!
//! The coverage gate ([`every_language_has_a_rule_or_explicit_deferral`]) walks
//! [`ALL_LANGUAGES`] so adding a new `Language` variant forces either a rule (in
//! `TEST_RULES`, with a fixture) or an explicit deferral entry (`NO_RULE_YET`).

use std::path::{Path, PathBuf};

use travsr_analysis::ParseOutput;
use travsr_core::{Language, TestRole, ALL_LANGUAGES};

/// One `(signature, expected role)` assertion within a fixture.
struct Case {
    /// Exact `node.vname.signature` to look up in the parse output.
    sig: &'static str,
    role: TestRole,
}

const fn case(sig: &'static str, role: TestRole) -> Case {
    Case { sig, role }
}

/// A language's golden fixture: the source file, the vname path it is parsed
/// under (path-sensitive rules — Go `_test.go`, Python `test_*.py`, TS
/// `*.test.ts`, Java `/src/test/java/` — key off this), and the assertions.
struct Fixture {
    lang: Language,
    file: &'static str,
    vname_path: &'static str,
    cases: &'static [Case],
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_role")
}

/// Dispatch to the language's own `parse()` — travsr-analysis has no unified
/// dispatch (each module owns its entry point).
fn parse_fixture(f: &Fixture) -> ParseOutput {
    let path = fixtures_dir().join(f.file);
    let vp = f.vname_path;
    match f.lang {
        Language::Rust => travsr_analysis::rust::parse("", &path, vp),
        Language::Go => travsr_analysis::go::parse("", &path, vp),
        Language::Python => travsr_analysis::python::parse("", &path, vp),
        Language::TypeScript => travsr_analysis::typescript::parse("", &path, vp),
        Language::Java => travsr_analysis::java::parse("", &path, vp),
        Language::CSharp => travsr_analysis::csharp::parse("", &path, vp),
        Language::Kotlin => travsr_analysis::kotlin::parse("", &path, vp),
        Language::Scala => travsr_analysis::scala::parse("", &path, vp),
        Language::Swift => travsr_analysis::swift::parse("", &path, vp),
        Language::Php => travsr_analysis::php::parse("", &path, vp),
        Language::Ruby => travsr_analysis::ruby::parse("", &path, vp),
        // Additional languages are wired in as their rules land (#479 §0.2).
        other => panic!("no parse dispatch wired for {other:?}"),
    }
    .unwrap_or_else(|e| panic!("{:?}: parse failed: {e}", f.lang))
}

/// The shipped per-language golden fixtures. Adding a language = add its fixture
/// file, add an entry here, and move it from `NO_RULE_YET` to `TEST_RULES`.
const FIXTURES: &[Fixture] = &[
    Fixture {
        lang: Language::Rust,
        file: "rust.rs",
        vname_path: "src/thing.rs",
        cases: &[
            case("fn:calibrate_works", TestRole::EntryPoint),
            case("fn:async_calibrate", TestRole::EntryPoint),
            case("fn:helper", TestRole::Support),
            case("fn:calibrate_semantic_floors", TestRole::None),
            case("fn:test_connection_pool", TestRole::None),
            case("struct:TestRunner", TestRole::None),
        ],
    },
    // Go: path-gated. The `_test.go` fixture carries the entry + support cases;
    // the production fixture carries the ordinary + adversarial `None` cases.
    Fixture {
        lang: Language::Go,
        file: "go_entry.go",
        vname_path: "pkg/foo_test.go",
        cases: &[
            case("fn:TestCalibrate", TestRole::EntryPoint),
            case("fn:helper", TestRole::Support),
        ],
    },
    Fixture {
        lang: Language::Go,
        file: "go_prod.go",
        vname_path: "pkg/server.go",
        cases: &[
            case("fn:CalibrateFloors", TestRole::None),
            case("fn:BenchmarkServer", TestRole::None),
        ],
    },
    // Python: corroborated by a TestCase scope or a pytest path.
    Fixture {
        lang: Language::Python,
        file: "python_entry.py",
        vname_path: "tests/test_calibrate.py",
        cases: &[
            case(
                "method:CalibrationTests.test_calibrate",
                TestRole::EntryPoint,
            ),
            case("method:CalibrationTests.helper", TestRole::Support),
            case("fn:test_module_level", TestRole::EntryPoint),
        ],
    },
    Fixture {
        lang: Language::Python,
        file: "python_prod.py",
        vname_path: "src/calibrate.py",
        cases: &[
            case("fn:calibrate_floors", TestRole::None),
            case("fn:test_connection_pool", TestRole::None),
        ],
    },
    // TypeScript: whole test file is a Support scope; no EntryPoint in v1.
    Fixture {
        lang: Language::TypeScript,
        file: "typescript_entry.ts",
        vname_path: "src/calibrate.test.ts",
        cases: &[
            case("class:CalibrationSuite", TestRole::Support),
            case("fn:setupFixture", TestRole::Support),
        ],
    },
    Fixture {
        lang: Language::TypeScript,
        file: "typescript_prod.ts",
        vname_path: "src/calibrate.ts",
        cases: &[
            case("fn:calibrateFloors", TestRole::None),
            case("fn:testConnectionPool", TestRole::None),
        ],
    },
    // Java: @Test annotation decisive; src/test/… path is a Support scope.
    Fixture {
        lang: Language::Java,
        file: "java_entry.java",
        vname_path: "src/test/java/com/foo/CalibrationTest.java",
        cases: &[
            case("method:CalibrationTest.calibrates", TestRole::EntryPoint),
            case("method:CalibrationTest.helper", TestRole::Support),
        ],
    },
    Fixture {
        lang: Language::Java,
        file: "java_prod.java",
        vname_path: "src/main/java/com/foo/Calibrator.java",
        cases: &[
            case("method:Calibrator.calibrateFloors", TestRole::None),
            case("method:Calibrator.testConnectionPool", TestRole::None),
        ],
    },
    // C#: [Test]/[Fact]/… attribute decisive; [TestFixture]/[TestClass] scope.
    Fixture {
        lang: Language::CSharp,
        file: "csharp.cs",
        vname_path: "src/Calibration.cs",
        cases: &[
            case("method:CalibrationTests.Calibrates", TestRole::EntryPoint),
            case("method:CalibrationTests.Helper", TestRole::Support),
            case("method:Calibrator.CalibrateFloors", TestRole::None),
            case("method:Calibrator.TestConnectionPool", TestRole::None),
        ],
    },
    // Kotlin: @Test annotation decisive; scope is path-based (Phase 2).
    Fixture {
        lang: Language::Kotlin,
        file: "kotlin.kt",
        vname_path: "src/Calibration.kt",
        cases: &[
            case("method:CalibrationTest.calibrates", TestRole::EntryPoint),
            case("fn:calibrateFloors", TestRole::None),
            case("fn:testConnectionPool", TestRole::None),
        ],
    },
    // Scala: @Test annotation decisive; scope is path-based (Phase 2).
    Fixture {
        lang: Language::Scala,
        file: "scala.scala",
        vname_path: "src/Calibration.scala",
        cases: &[
            case("method:CalibrationTest.calibrates", TestRole::EntryPoint),
            case("method:Calibrator.calibrateFloors", TestRole::None),
            case("method:Calibrator.testConnectionPool", TestRole::None),
        ],
    },
    // Swift: swift-testing @Test decisive; XCTestCase subclass is a Support scope.
    Fixture {
        lang: Language::Swift,
        file: "swift.swift",
        vname_path: "Sources/Calibration.swift",
        cases: &[
            case("fn:calibrates", TestRole::EntryPoint),
            case("method:CalibrationTests.testCalibrate", TestRole::Support),
            case("method:CalibrationTests.helper", TestRole::Support),
            case("fn:calibrateFloors", TestRole::None),
            case("fn:testConnectionPool", TestRole::None),
        ],
    },
    // PHP: #[Test] attribute decisive; `extends TestCase` is a Support scope.
    Fixture {
        lang: Language::Php,
        file: "php.php",
        vname_path: "src/Calibration.php",
        cases: &[
            case("method:CalibrationTest.calibrates", TestRole::EntryPoint),
            case("method:CalibrationTest.helper", TestRole::Support),
            case("method:Calibrator.calibrateFloors", TestRole::None),
            case("method:Calibrator.testConnectionPool", TestRole::None),
        ],
    },
    // Ruby: Minitest/Test::Unit subclass is a Support scope; `def test_*` inside
    // is the EntryPoint.
    Fixture {
        lang: Language::Ruby,
        file: "ruby.rb",
        vname_path: "lib/calibration.rb",
        cases: &[
            case(
                "method:CalibrationTest.test_calibrate",
                TestRole::EntryPoint,
            ),
            case("method:CalibrationTest.helper", TestRole::Support),
            case("method:Calibrator.calibrate_floors", TestRole::None),
            case("method:Calibrator.test_connection_pool", TestRole::None),
        ],
    },
];

#[test]
fn golden_test_roles() {
    for f in FIXTURES {
        let out = parse_fixture(f);
        let have: Vec<&str> = out
            .nodes
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        for c in f.cases {
            let node = out
                .nodes
                .iter()
                .find(|n| n.vname.signature == c.sig)
                .unwrap_or_else(|| {
                    panic!("{:?}: missing node `{}` — have {have:?}", f.lang, c.sig)
                });
            assert_eq!(
                node.test_role, c.role,
                "{:?}: `{}` expected {:?}, got {:?}",
                f.lang, c.sig, c.role, node.test_role
            );
        }
    }
}

// ── Coverage gate (§7.2) ─────────────────────────────────────────────────────

/// Languages with a shipped test-role rule **and** a golden fixture above.
const TEST_RULES: &[Language] = &[
    Language::Rust,
    Language::Go,
    Language::Python,
    Language::TypeScript,
    Language::Java,
    Language::CSharp,
    Language::Kotlin,
    Language::Scala,
    Language::Swift,
    Language::Php,
    Language::Ruby,
];

/// Languages intentionally without a rule yet.
///
/// Two kinds live here:
/// - **Permanent N/A:** data/config/prose formats (no functions to classify).
/// - **Pending (#479 §0.2):** code languages still awaiting a rule. Each is a
///   tracked TODO; moving it to `TEST_RULES` (with a fixture) clears it.
const NO_RULE_YET: &[Language] = &[
    // Pending — #479 §0.2. These four have no decisive AST signal (idiomatic
    // detection is path-based: `_test.c`/`/test/`, `_test.dart`, GoogleTest
    // macros, ObjC XCTestCase whose superclass is on a *separate* @interface
    // node than its @implementation methods), so they await a Phase-1 capture
    // rule and are uncategorized until then.
    Language::Cpp,
    Language::C,
    Language::Dart,
    Language::ObjectiveC,
    // Permanent N/A — no functions, so no test entry points.
    Language::Json,
    Language::Yaml,
    Language::Toml,
    Language::Xml,
    Language::Markdown,
];

#[test]
fn every_language_has_a_rule_or_explicit_deferral() {
    for &lang in ALL_LANGUAGES {
        let covered = TEST_RULES.contains(&lang) || NO_RULE_YET.contains(&lang);
        assert!(
            covered,
            "Language::{} has no test-detection rule and is not on the deferral list \
             (add a rule to TEST_RULES with a fixture, or defer it in NO_RULE_YET)",
            lang.as_str()
        );
    }
    // A language cannot be on both lists.
    for &lang in TEST_RULES {
        assert!(
            !NO_RULE_YET.contains(&lang),
            "Language::{} is in both TEST_RULES and NO_RULE_YET",
            lang.as_str()
        );
    }
    // Every language claiming a rule must actually ship a fixture.
    for &lang in TEST_RULES {
        assert!(
            FIXTURES.iter().any(|f| f.lang == lang),
            "Language::{} is in TEST_RULES but has no golden fixture",
            lang.as_str()
        );
    }
}
